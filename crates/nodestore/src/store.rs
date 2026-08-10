//! The MVCC store: an append-only history in sqlite, with etcd's revision
//! semantics on top.
//!
//! # Shape of the data
//!
//! One row per write, ever — no row is ever updated in place, and the current
//! state of a key is simply its highest-revision row. That is what makes
//! historical reads (`Range` at a past revision) and watch replay the same
//! query with a different bound, instead of two mechanisms:
//!
//! ```text
//!   revision sub key      value  create_rev version lease deleted prev_rev
//!   2        0   /a       v1     2          1       0     0       0
//!   3        0   /b       v1     3          1       0     0       0
//!   4        0   /a       v2     2          2       0     0       2      <- update
//!   5        0   /a       NULL   0          0       0     1       4      <- tombstone
//! ```
//!
//! `prev_rev` points at the row this one superseded, which is how `prev_kv`
//! is served without storing the old value twice. kube-apiserver's watch sets
//! `WithPrevKV()`, and a DELETE event's payload *is* the previous value — get
//! this wrong and every watcher sees deletions of empty objects.
//!
//! # Revisions
//!
//! One counter for the whole store, not per key. It increments **once per
//! applied command that actually writes something** — a `DeleteRange` matching
//! nothing does not burn a revision, and neither does a transaction whose
//! chosen branch is empty. Multiple writes from one transaction share the main
//! revision and are ordered by `sub`, exactly as etcd does it.
//!
//! An empty store is at revision 1, so the first write produces revision 2.
//!
//! # Concurrency
//!
//! [`Store`] is deliberately not internally synchronized: it is owned by the
//! consensus layer, which serializes every mutation through a single applier.
//! Reads take the same lock. On the volume a single-node control plane
//! generates, a mutex around a local sqlite file is far cheaper than the bugs
//! a connection pool with concurrent writers would buy — and under raft the
//! applier is single-threaded by definition anyway.

use crate::command::{
    Command, Compare, CompareResult, CompareTarget, DeleteOp, KeyRange, PutOp, RangeQuery,
    RequestOp, Sort, SortTarget, TxnOp,
};
use crate::error::{Error, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Transaction};
use std::path::Path;

/// A key/value pair as etcd reports it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct KeyValue {
    pub key: Vec<u8>,
    pub create_revision: i64,
    pub mod_revision: i64,
    pub version: i64,
    pub value: Vec<u8>,
    pub lease: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Put,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub kv: KeyValue,
    pub prev_kv: Option<KeyValue>,
}

/// Result of a range read.
#[derive(Clone, Debug, Default)]
pub struct RangeResult {
    pub kvs: Vec<KeyValue>,
    /// Total matching keys, ignoring `limit` — etcd reports this so a client
    /// can tell a truncated page from a complete one.
    pub count: i64,
    pub more: bool,
}

/// What an applied command produced, beyond its events.
#[derive(Clone, Debug)]
pub enum CommandResponse {
    Put { prev_kv: Option<KeyValue> },
    Delete { deleted: i64, prev_kvs: Vec<KeyValue> },
    Txn { succeeded: bool, responses: Vec<OpResponse> },
    Compact,
    Lease { ttl_secs: i64 },
    Empty,
}

#[derive(Clone, Debug)]
pub enum OpResponse {
    Range(RangeResult),
    Put { prev_kv: Option<KeyValue> },
    Delete { deleted: i64, prev_kvs: Vec<KeyValue> },
}

/// The whole state machine at one applied index.
#[derive(Clone, Debug, Default)]
pub struct SnapshotState {
    pub revision: i64,
    pub compact_revision: i64,
    pub applied_index: u64,
    pub kvs: Vec<KeyValue>,
    /// (id, ttl_secs, expires_at)
    pub leases: Vec<(i64, i64, i64)>,
    pub members: Vec<crate::command::Member>,
}

/// The outcome of applying one command.
#[derive(Clone, Debug)]
pub struct Applied {
    /// Store revision *after* the command — what goes in the response header.
    pub revision: i64,
    pub response: CommandResponse,
    pub events: Vec<Event>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database at `path`, or an in-memory store
    /// for `":memory:"`.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = if path == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            Connection::open(path)?
        };
        let store = Store { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        // WAL: readers never block the applier, which matters because every
        // watch replay is a read racing the write path.
        //
        // synchronous=NORMAL rather than FULL: in WAL mode this can lose the
        // tail of the last transaction on power loss but never corrupts the
        // database. That is the right trade for a control-plane store on a
        // device with no battery-backed cache — and once raft lands, a
        // replica's lost tail is recovered from the leader's log, which is
        // precisely the durability model raft exists to provide.
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS kv (
                revision        INTEGER NOT NULL,
                sub             INTEGER NOT NULL,
                key             BLOB    NOT NULL,
                value           BLOB,
                create_revision INTEGER NOT NULL,
                version         INTEGER NOT NULL,
                lease           INTEGER NOT NULL DEFAULT 0,
                deleted         INTEGER NOT NULL DEFAULT 0,
                prev_revision   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (revision, sub)
            );

            -- The lookup behind every point read and every prev_kv: newest
            -- row for a key, or the row at an exact revision.
            CREATE INDEX IF NOT EXISTS kv_key_revision ON kv (key, revision);
            -- Watch replay scans forward by revision.
            CREATE INDEX IF NOT EXISTS kv_revision ON kv (revision);
            -- Lease expiry needs every live key held by a lease.
            CREATE INDEX IF NOT EXISTS kv_lease ON kv (lease) WHERE lease != 0;

            CREATE TABLE IF NOT EXISTS meta (
                name  TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS lease (
                id         INTEGER PRIMARY KEY,
                ttl_secs   INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS lease_expires ON lease (expires_at);

            -- The cluster's address book. In the state machine, not in
            -- configuration, so a member that joins later or restarts learns
            -- the cluster's shape from state it has to catch up on anyway
            -- (see command.rs's SetMember).
            CREATE TABLE IF NOT EXISTS member (
                id         INTEGER PRIMARY KEY,
                peer_url   TEXT NOT NULL,
                client_url TEXT NOT NULL,
                name       TEXT NOT NULL,
                is_learner INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        // An empty store sits at revision 1, matching etcd: the first write
        // is revision 2, and 1 is a legal "start from the beginning" watch.
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (name, value) VALUES ('revision', 1), ('compact_revision', 0)",
            [],
        )?;
        Ok(())
    }

    pub fn revision(&self) -> Result<i64> {
        Ok(self.meta("revision")?)
    }

    pub fn compact_revision(&self) -> Result<i64> {
        Ok(self.meta("compact_revision")?)
    }

    fn meta(&self, name: &str) -> Result<i64> {
        let v: i64 = self
            .conn
            .query_row("SELECT value FROM meta WHERE name = ?1", [name], |r| r.get(0))?;
        Ok(v)
    }

    // ── Reads ────────────────────────────────────────────────────────────

    /// Read a range, optionally as of a past revision.
    ///
    /// A read at a revision at or below the compaction point fails: the
    /// history needed to answer it is gone, and answering from what survives
    /// would silently return a *different* result than the client asked for.
    pub fn range(&self, q: &RangeQuery) -> Result<RangeResult> {
        let current = self.revision()?;
        let at = if q.revision <= 0 { current } else { q.revision };
        if q.revision > 0 {
            let compacted = self.compact_revision()?;
            if q.revision <= compacted {
                return Err(Error::Compacted { compact_revision: compacted });
            }
            if q.revision > current {
                return Err(Error::FutureRevision { requested: q.revision, current });
            }
        }
        // Same code path a transaction's own Range op takes, so a read is
        // answered identically whether or not it is inside a txn.
        range_in(&self.conn, &q.range, at, q)
    }

    /// Events strictly after `since`, in apply order — the watch replay path.
    pub fn events_since(&self, since: i64, range: &KeyRange) -> Result<Vec<(i64, Event)>> {
        let compacted = self.compact_revision()?;
        if since < compacted {
            return Err(Error::Compacted { compact_revision: compacted });
        }
        let (pred, mut args) = range_predicate(range);
        args.push(SqlValue::Integer(since));
        let sql = format!(
            "SELECT revision, sub, key, value, create_revision, version, lease, deleted, prev_revision
             FROM kv WHERE {pred} AND revision > ?{n} ORDER BY revision ASC, sub ASC",
            pred = pred,
            n = args.len()
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (revision, key, value, create_revision, version, lease, deleted, prev_revision) = row?;
            let prev_kv = self.row_at(&self.conn, &key, prev_revision)?;
            let event = if deleted != 0 {
                Event {
                    kind: EventKind::Delete,
                    // etcd's delete event carries the key and the revision of
                    // the deletion, with everything else zeroed.
                    kv: KeyValue { key, mod_revision: revision, ..Default::default() },
                    prev_kv,
                }
            } else {
                Event {
                    kind: EventKind::Put,
                    kv: KeyValue {
                        key,
                        value: value.unwrap_or_default(),
                        create_revision,
                        mod_revision: revision,
                        version,
                        lease,
                    },
                    prev_kv,
                }
            };
            out.push((revision, event));
        }
        Ok(out)
    }

    /// The row for `key` at exactly `revision`, used to resolve `prev_kv`.
    /// A tombstone resolves to `None`: there was no previous *value*.
    fn row_at(&self, conn: &Connection, key: &[u8], revision: i64) -> Result<Option<KeyValue>> {
        if revision == 0 {
            return Ok(None);
        }
        let row = conn
            .query_row(
                "SELECT key, value, create_revision, revision, version, lease, deleted
                 FROM kv WHERE key = ?1 AND revision = ?2",
                rusqlite::params![key, revision],
                |row| {
                    let deleted: i64 = row.get(6)?;
                    Ok((
                        KeyValue {
                            key: row.get(0)?,
                            value: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
                            create_revision: row.get(2)?,
                            mod_revision: row.get(3)?,
                            version: row.get(4)?,
                            lease: row.get(5)?,
                        },
                        deleted,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((kv, 0)) => Some(kv),
            _ => None,
        })
    }

    pub fn lease_ttl(&self, id: i64) -> Result<Option<(i64, i64)>> {
        let row = self
            .conn
            .query_row("SELECT ttl_secs, expires_at FROM lease WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        Ok(row)
    }

    pub fn lease_keys(&self, id: i64) -> Result<Vec<Vec<u8>>> {
        let mut stmt = self.conn.prepare(
            "SELECT key FROM (
                 SELECT key, lease, deleted,
                        ROW_NUMBER() OVER (PARTITION BY key ORDER BY revision DESC, sub DESC) AS rn
                 FROM kv
             ) WHERE rn = 1 AND deleted = 0 AND lease = ?1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, Vec<u8>>(0))?;
        let mut keys = Vec::new();
        for k in rows {
            keys.push(k?);
        }
        Ok(keys)
    }

    /// Every member in the replicated address book.
    pub fn members(&self) -> Result<Vec<crate::command::Member>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, peer_url, client_url, name, is_learner FROM member ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::command::Member {
                id: r.get::<_, i64>(0)? as u64,
                peer_url: r.get(1)?,
                client_url: r.get(2)?,
                name: r.get(3)?,
                is_learner: r.get::<_, i64>(4)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for m in rows {
            out.push(m?);
        }
        Ok(out)
    }

    /// One member, by id — how a follower turns "the leader is id 3" into a
    /// URL it can forward to.
    pub fn member(&self, id: u64) -> Result<Option<crate::command::Member>> {
        Ok(self.members()?.into_iter().find(|m| m.id == id))
    }

    pub fn leases(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM lease")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(id?);
        }
        Ok(ids)
    }

    /// Leases that have expired as of `now_unix_secs`. Read-only: the leader
    /// calls this to decide *whether* to propose an expiry command, and the
    /// command is what actually deletes anything.
    pub fn expired_leases(&self, now_unix_secs: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM lease WHERE expires_at <= ?1")?;
        let rows = stmt.query_map([now_unix_secs], |r| r.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(id?);
        }
        Ok(ids)
    }

    // ── Snapshots ────────────────────────────────────────────────────────

    /// Everything a replica needs to reconstruct this state machine.
    ///
    /// Live keys only — no history. A restoring follower could not serve a
    /// read or a watch from before the snapshot anyway (its log starts at the
    /// snapshot's index), so carrying the history would make a snapshot
    /// unbounded in the one situation where it has to be quick.
    pub fn export_snapshot(&self) -> Result<SnapshotState> {
        let all = range_in(
            &self.conn,
            &KeyRange::All,
            self.revision()?,
            &RangeQuery::current(KeyRange::All),
        )?;
        let mut leases = Vec::new();
        {
            let mut stmt = self.conn.prepare("SELECT id, ttl_secs, expires_at FROM lease")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
            })?;
            for row in rows {
                leases.push(row?);
            }
        }
        Ok(SnapshotState {
            revision: self.revision()?,
            compact_revision: self.compact_revision()?,
            applied_index: self.applied_index()?,
            kvs: all.kvs,
            leases,
            members: self.members()?,
        })
    }

    /// Replace this state machine wholesale with a snapshot.
    ///
    /// Destructive by necessity: a follower being sent a snapshot is one whose
    /// own state is *known* to be unreconstructable from the log, so merging
    /// would preserve exactly the rows that must not survive. Everything
    /// happens in one transaction, so a crash midway leaves the old state
    /// rather than a hybrid of two.
    ///
    /// Restored keys keep their original revisions, which is what makes a
    /// restored replica answer reads identically to the one that sent it.
    pub fn restore_snapshot(&mut self, snapshot: &SnapshotState) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM kv", [])?;
        tx.execute("DELETE FROM lease", [])?;
        tx.execute("DELETE FROM member", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO kv (revision, sub, key, value, create_revision, version, lease, deleted, prev_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0)",
            )?;
            // `sub` is allocated per revision, not hardcoded to 0. One applied
            // transaction writes several keys at the *same* revision with
            // different subs, so a snapshot of such a store has repeated
            // mod_revisions — and with the primary key being (revision, sub),
            // a second row at sub 0 fails the UNIQUE constraint and rolls the
            // whole restore back. A follower past the compaction point would
            // then have no recovery path left at all, snapshots being the last
            // one.
            //
            // export_snapshot reads in the default key order, so this
            // assignment is deterministic across replicas.
            let mut last_revision = i64::MIN;
            let mut sub = 0i64;
            for kv in &snapshot.kvs {
                if kv.mod_revision == last_revision {
                    sub += 1;
                } else {
                    last_revision = kv.mod_revision;
                    sub = 0;
                }
                stmt.execute(rusqlite::params![
                    kv.mod_revision,
                    sub,
                    kv.key,
                    kv.value,
                    kv.create_revision,
                    kv.version,
                    kv.lease,
                ])?;
            }
        }
        {
            let mut stmt =
                tx.prepare("INSERT INTO lease (id, ttl_secs, expires_at) VALUES (?1, ?2, ?3)")?;
            for (id, ttl, expires) in &snapshot.leases {
                stmt.execute(rusqlite::params![id, ttl, expires])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO member (id, peer_url, client_url, name, is_learner)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for m in &snapshot.members {
                stmt.execute(rusqlite::params![
                    m.id as i64,
                    m.peer_url,
                    m.client_url,
                    m.name,
                    m.is_learner as i64
                ])?;
            }
        }
        tx.execute("UPDATE meta SET value = ?1 WHERE name = 'revision'", [snapshot.revision])?;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE name = 'compact_revision'",
            [snapshot.compact_revision],
        )?;
        // In the same transaction as the state it describes — the invariant
        // the whole crash-recovery story rests on (see replication/log.rs).
        tx.execute(
            "INSERT INTO meta (name, value) VALUES ('applied_index', ?1)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            [snapshot.applied_index as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The raft index this state machine has applied up to.
    pub fn applied_index(&self) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE name = 'applied_index'", [], |r| r.get(0))
            .optional()?;
        Ok(v.unwrap_or(0) as u64)
    }

    // ── Apply ────────────────────────────────────────────────────────────

    /// Apply one command. The only way anything is ever written.
    ///
    /// Everything happens in a single sqlite transaction, so a command is
    /// atomic even when it writes many keys — a transaction that half-applied
    /// would put a replica permanently out of step with the leader.
    pub fn apply(&mut self, cmd: &Command) -> Result<Applied> {
        self.apply_at(0, cmd)
    }

    /// Apply a command as raft log index `index`.
    ///
    /// The index is written in the *same transaction* as the state it
    /// produced. That is not bookkeeping tidiness: it is what makes losing
    /// the tail of this database recoverable. Both roll back together,
    /// leaving an older but consistent replica the log can catch up. Stored
    /// separately, a crash between the two would leave a replica believing it
    /// applied an entry it did not, which replay cannot fix — see
    /// replication/log.rs.
    ///
    /// `index` 0 means "not driven by raft" (single-node, and the unit tests),
    /// in which case no index is recorded.
    pub fn apply_at(&mut self, index: u64, cmd: &Command) -> Result<Applied> {
        let current = self.revision()?;
        let tx = self.conn.transaction()?;
        let mut w = Writer { next: current + 1, sub: 0, used: false, events: Vec::new() };

        let response = match cmd {
            Command::Put(op) => {
                let prev_kv = apply_put(&tx, &mut w, op)?;
                CommandResponse::Put { prev_kv }
            }
            Command::Delete(op) => {
                let (deleted, prev_kvs) = apply_delete(&tx, &mut w, op)?;
                CommandResponse::Delete { deleted, prev_kvs }
            }
            Command::Txn(op) => {
                let (succeeded, responses) = apply_txn(&tx, &mut w, op, current)?;
                CommandResponse::Txn { succeeded, responses }
            }
            Command::Compact { revision } => {
                apply_compact(&tx, *revision, current)?;
                CommandResponse::Compact
            }
            Command::LeaseGrant { id, ttl_secs, now_unix_secs } => {
                // etcd clamps a non-positive TTL to its minimum rather than
                // creating a lease that is already expired.
                let ttl = (*ttl_secs).max(1);
                // The clock starts at grant, not at the first keepalive — a
                // lease nobody ever renews must still expire.
                tx.execute(
                    "INSERT OR REPLACE INTO lease (id, ttl_secs, expires_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, ttl, now_unix_secs + ttl],
                )?;
                CommandResponse::Lease { ttl_secs: ttl }
            }
            Command::LeaseKeepAlive { id, now_unix_secs } => {
                let ttl: Option<i64> = tx
                    .query_row("SELECT ttl_secs FROM lease WHERE id = ?1", [id], |r| r.get(0))
                    .optional()?;
                match ttl {
                    // etcd answers a keepalive for an unknown lease with
                    // TTL 0 rather than an error; the client treats that as
                    // "your lease is gone".
                    None => CommandResponse::Lease { ttl_secs: 0 },
                    Some(ttl) => {
                        tx.execute(
                            "UPDATE lease SET expires_at = ?2 WHERE id = ?1",
                            rusqlite::params![id, now_unix_secs + ttl],
                        )?;
                        CommandResponse::Lease { ttl_secs: ttl }
                    }
                }
            }
            Command::LeaseRevoke { id } => {
                revoke_lease(&tx, &mut w, *id)?;
                CommandResponse::Empty
            }
            // Membership is cluster metadata, not user data: it produces no
            // kv events and does not advance the store revision. etcd behaves
            // the same way — adding a member must not look to a watching
            // client like something changed under /registry.
            Command::SetMember(m) => {
                tx.execute(
                    "INSERT INTO member (id, peer_url, client_url, name, is_learner)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET peer_url = excluded.peer_url,
                                                   client_url = excluded.client_url,
                                                   name = excluded.name,
                                                   is_learner = excluded.is_learner",
                    rusqlite::params![m.id as i64, m.peer_url, m.client_url, m.name, m.is_learner as i64],
                )?;
                CommandResponse::Empty
            }
            Command::RemoveMember { id } => {
                tx.execute("DELETE FROM member WHERE id = ?1", [*id as i64])?;
                CommandResponse::Empty
            }
            Command::ExpireLeases { now_unix_secs } => {
                let mut stmt = tx.prepare("SELECT id FROM lease WHERE expires_at <= ?1")?;
                let ids: Vec<i64> = stmt
                    .query_map([now_unix_secs], |r| r.get::<_, i64>(0))?
                    .collect::<std::result::Result<_, _>>()?;
                drop(stmt);
                for id in ids {
                    revoke_lease(&tx, &mut w, id)?;
                }
                CommandResponse::Empty
            }
        };

        // The revision only advances if something was actually written. etcd
        // is specific about this: a delete that matched nothing leaves the
        // store revision alone, and a client that saw it move would conclude
        // it had missed an event.
        let revision = if w.used {
            tx.execute("UPDATE meta SET value = ?1 WHERE name = 'revision'", [w.next])?;
            w.next
        } else {
            current
        };
        if index > 0 {
            tx.execute(
                "INSERT INTO meta (name, value) VALUES ('applied_index', ?1)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                [index as i64],
            )?;
        }
        tx.commit()?;
        Ok(Applied { revision, response, events: w.events })
    }
}

/// Revision bookkeeping for one `apply`: the revision this command will use if
/// it writes anything, and the sub-counter ordering writes within it.
struct Writer {
    next: i64,
    sub: i64,
    used: bool,
    events: Vec<Event>,
}

impl Writer {
    fn take_sub(&mut self) -> i64 {
        self.used = true;
        let s = self.sub;
        self.sub += 1;
        s
    }
}

fn apply_put(tx: &Transaction<'_>, w: &mut Writer, op: &PutOp) -> Result<Option<KeyValue>> {
    let prev = current_in(tx, &op.key)?;

    // ignore_value/ignore_lease are etcd's "change one field, keep the other"
    // forms. Both are errors on a key that doesn't exist — there is nothing to
    // keep.
    if (op.ignore_value || op.ignore_lease) && prev.is_none() {
        return Err(Error::KeyNotFound);
    }
    let value = if op.ignore_value {
        prev.as_ref().map(|p| p.value.clone()).unwrap_or_default()
    } else {
        op.value.clone()
    };
    let lease = if op.ignore_lease {
        prev.as_ref().map(|p| p.lease).unwrap_or(0)
    } else {
        op.lease
    };

    let (create_revision, version, prev_revision) = match &prev {
        // A key re-created after deletion starts over at version 1 with a new
        // create_revision — it is a different object as far as etcd, and
        // therefore as far as apiserver's optimistic concurrency, is
        // concerned.
        None => (w.next, 1, 0),
        Some(p) => (p.create_revision, p.version + 1, p.mod_revision),
    };

    let sub = w.take_sub();
    tx.execute(
        "INSERT INTO kv (revision, sub, key, value, create_revision, version, lease, deleted, prev_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
        rusqlite::params![w.next, sub, op.key, value, create_revision, version, lease, prev_revision],
    )?;

    let kv = KeyValue {
        key: op.key.clone(),
        value,
        create_revision,
        mod_revision: w.next,
        version,
        lease,
    };
    w.events.push(Event { kind: EventKind::Put, kv, prev_kv: prev.clone() });
    Ok(if op.prev_kv { prev } else { None })
}

fn apply_delete(
    tx: &Transaction<'_>,
    w: &mut Writer,
    op: &DeleteOp,
) -> Result<(i64, Vec<KeyValue>)> {
    let victims = current_range_in(tx, &op.range)?;
    // Counted up front, not by filtering w.events afterwards: within a
    // transaction the event list also holds earlier ops' deletes at this same
    // revision, and counting those would report a delete of keys this op
    // never touched.
    let deleted = victims.len() as i64;
    let mut prev_kvs = Vec::new();
    for prev in victims {
        let sub = w.take_sub();
        tx.execute(
            "INSERT INTO kv (revision, sub, key, value, create_revision, version, lease, deleted, prev_revision)
             VALUES (?1, ?2, ?3, NULL, 0, 0, 0, 1, ?4)",
            rusqlite::params![w.next, sub, prev.key, prev.mod_revision],
        )?;
        w.events.push(Event {
            kind: EventKind::Delete,
            kv: KeyValue { key: prev.key.clone(), mod_revision: w.next, ..Default::default() },
            prev_kv: Some(prev.clone()),
        });
        if op.prev_kv {
            prev_kvs.push(prev);
        }
    }
    Ok((deleted, prev_kvs))
}

fn apply_txn(
    tx: &Transaction<'_>,
    w: &mut Writer,
    op: &TxnOp,
    current: i64,
) -> Result<(bool, Vec<OpResponse>)> {
    let succeeded = op.compare.iter().try_fold(true, |acc, c| -> Result<bool> {
        Ok(acc && evaluate_compare(tx, c)?)
    })?;
    let branch = if succeeded { &op.success } else { &op.failure };

    let mut responses = Vec::new();
    for req in branch {
        match req {
            RequestOp::Put(p) => {
                let prev_kv = apply_put(tx, w, p)?;
                responses.push(OpResponse::Put { prev_kv });
            }
            RequestOp::Delete(d) => {
                let (deleted, prev_kvs) = apply_delete(tx, w, d)?;
                responses.push(OpResponse::Delete { deleted, prev_kvs });
            }
            RequestOp::Range(q) => {
                // Reads inside a transaction see that transaction's own
                // writes, and default to the revision this command will
                // produce rather than the one before it.
                let at = if q.revision > 0 { q.revision } else if w.used { w.next } else { current };
                responses.push(OpResponse::Range(range_in(tx, &q.range, at, q)?));
            }
        }
    }
    Ok((succeeded, responses))
}

/// etcd compares against an all-zero key when the key does not exist, which is
/// what makes `Compare(ModRevision(key), Equal, 0)` the idiom for "create only
/// if absent". Treating a missing key as "no answer" instead would turn every
/// create into a failed transaction.
fn evaluate_compare(tx: &Transaction<'_>, c: &Compare) -> Result<bool> {
    let kv = current_in(tx, &c.key)?.unwrap_or_default();
    let ordering = match &c.target {
        CompareTarget::Version(v) => kv.version.cmp(v),
        CompareTarget::CreateRevision(v) => kv.create_revision.cmp(v),
        CompareTarget::ModRevision(v) => kv.mod_revision.cmp(v),
        CompareTarget::Lease(v) => kv.lease.cmp(v),
        CompareTarget::Value(v) => kv.value.cmp(v),
    };
    Ok(match c.result {
        CompareResult::Equal => ordering.is_eq(),
        CompareResult::NotEqual => ordering.is_ne(),
        CompareResult::Greater => ordering.is_gt(),
        CompareResult::Less => ordering.is_lt(),
    })
}

fn apply_compact(tx: &Transaction<'_>, revision: i64, current: i64) -> Result<()> {
    let compacted: i64 =
        tx.query_row("SELECT value FROM meta WHERE name = 'compact_revision'", [], |r| r.get(0))?;
    if revision <= compacted {
        return Err(Error::Compacted { compact_revision: compacted });
    }
    if revision > current {
        return Err(Error::FutureRevision { requested: revision, current });
    }

    // Drop superseded history at or below the compaction point, but never the
    // row that represents a key's current state — a live key must still be
    // readable at the current revision after its older versions are gone.
    tx.execute(
        "DELETE FROM kv WHERE revision <= ?1 AND rowid NOT IN (
             SELECT rowid FROM (
                 SELECT rowid, ROW_NUMBER() OVER (PARTITION BY key ORDER BY revision DESC, sub DESC) AS rn
                 FROM kv WHERE revision <= ?1
             ) WHERE rn = 1
         )",
        [revision],
    )?;
    // Tombstones below the compaction point are the exception: nothing needs
    // them once no watch can start early enough to see the deletion.
    tx.execute("DELETE FROM kv WHERE revision <= ?1 AND deleted = 1", [revision])?;
    tx.execute("UPDATE meta SET value = ?1 WHERE name = 'compact_revision'", [revision])?;
    Ok(())
}

fn revoke_lease(tx: &Transaction<'_>, w: &mut Writer, id: i64) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT key FROM (
             SELECT key, lease, deleted,
                    ROW_NUMBER() OVER (PARTITION BY key ORDER BY revision DESC, sub DESC) AS rn
             FROM kv
         ) WHERE rn = 1 AND deleted = 0 AND lease = ?1 ORDER BY key",
    )?;
    let keys: Vec<Vec<u8>> =
        stmt.query_map([id], |r| r.get::<_, Vec<u8>>(0))?.collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    for key in keys {
        apply_delete(tx, w, &DeleteOp { range: KeyRange::Single(key), prev_kv: false })?;
    }
    tx.execute("DELETE FROM lease WHERE id = ?1", [id])?;
    Ok(())
}

// ── Free-standing query helpers ──────────────────────────────────────────
//
// Duplicated in spirit with Store's own methods, but taking a &Connection so
// they work against a live Transaction. Rust's borrow rules make sharing the
// method bodies here more trouble than the handful of lines is worth.

fn current_in(conn: &Connection, key: &[u8]) -> Result<Option<KeyValue>> {
    let row = conn
        .query_row(
            "SELECT key, value, create_revision, revision, version, lease, deleted
             FROM kv WHERE key = ?1 ORDER BY revision DESC, sub DESC LIMIT 1",
            [key],
            |row| {
                let deleted: i64 = row.get(6)?;
                Ok((
                    KeyValue {
                        key: row.get(0)?,
                        value: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
                        create_revision: row.get(2)?,
                        mod_revision: row.get(3)?,
                        version: row.get(4)?,
                        lease: row.get(5)?,
                    },
                    deleted,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((kv, 0)) => Some(kv),
        _ => None,
    })
}

fn current_range_in(conn: &Connection, range: &KeyRange) -> Result<Vec<KeyValue>> {
    let q = RangeQuery::current(range.clone());
    Ok(range_in(conn, range, i64::MAX, &q)?.kvs)
}

fn range_in(conn: &Connection, range: &KeyRange, at: i64, q: &RangeQuery) -> Result<RangeResult> {
    let (pred, mut args) = range_predicate(range);
    args.push(SqlValue::Integer(at));
    let base = format!(
        "SELECT key, value, create_revision, revision, version, lease, deleted FROM (
             SELECT key, value, create_revision, revision, version, lease, deleted,
                    ROW_NUMBER() OVER (PARTITION BY key ORDER BY revision DESC, sub DESC) AS rn
             FROM kv WHERE {pred} AND revision <= ?{n}
         ) WHERE rn = 1 AND deleted = 0",
        pred = pred,
        n = args.len()
    );
    let count: i64 = {
        let sql = format!("SELECT COUNT(*) FROM ({base})");
        conn.query_row(&sql, params_from_iter(args.iter()), |r| r.get(0))?
    };
    if q.count_only {
        return Ok(RangeResult { kvs: Vec::new(), count, more: false });
    }
    let mut sql = format!("{base} ORDER BY {}", order_by(q.sort));
    if q.limit > 0 {
        sql.push_str(&format!(" LIMIT {}", q.limit + 1));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
        Ok(KeyValue {
            key: row.get(0)?,
            value: if q.keys_only {
                Vec::new()
            } else {
                row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default()
            },
            create_revision: row.get(2)?,
            mod_revision: row.get(3)?,
            version: row.get(4)?,
            lease: row.get(5)?,
        })
    })?;
    let mut kvs = Vec::new();
    for kv in rows {
        kvs.push(kv?);
    }
    let more = q.limit > 0 && kvs.len() as i64 > q.limit;
    if more {
        kvs.truncate(q.limit as usize);
    }
    Ok(RangeResult { kvs, count, more })
}

/// SQL predicate + bind values for a key range. Keys are BLOBs, so sqlite
/// compares them bytewise — the same ordering etcd defines.
fn range_predicate(range: &KeyRange) -> (String, Vec<SqlValue>) {
    match range {
        KeyRange::All => ("1 = 1".to_string(), Vec::new()),
        KeyRange::Single(k) => ("key = ?1".to_string(), vec![SqlValue::Blob(k.clone())]),
        KeyRange::From(f) => ("key >= ?1".to_string(), vec![SqlValue::Blob(f.clone())]),
        KeyRange::Between { from, to } => (
            "key >= ?1 AND key < ?2".to_string(),
            vec![SqlValue::Blob(from.clone()), SqlValue::Blob(to.clone())],
        ),
    }
}

fn order_by(sort: Option<Sort>) -> String {
    let Some(sort) = sort else {
        // etcd's default, and the only order apiserver's paging is correct
        // under.
        return "key ASC".to_string();
    };
    let column = match sort.target {
        SortTarget::Key => "key",
        SortTarget::Version => "version",
        SortTarget::CreateRevision => "create_revision",
        SortTarget::ModRevision => "revision",
        SortTarget::Value => "value",
    };
    let direction = if sort.ascending { "ASC" } else { "DESC" };
    // Key is the tiebreaker so a sort on a non-unique column is still a total
    // order — otherwise paging through it can repeat or skip rows.
    format!("{column} {direction}, key ASC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::*;

    fn store() -> Store {
        Store::open(Path::new(":memory:")).expect("open in-memory store")
    }

    fn put(s: &mut Store, key: &str, value: &str) -> Applied {
        s.apply(&Command::Put(PutOp {
            key: key.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
            lease: 0,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        }))
        .expect("put")
    }

    fn get(s: &Store, key: &str) -> Option<KeyValue> {
        s.range(&RangeQuery::current(KeyRange::Single(key.as_bytes().to_vec())))
            .expect("range")
            .kvs
            .into_iter()
            .next()
    }

    // ── Revisions ────────────────────────────────────────────────────────

    #[test]
    fn an_empty_store_starts_at_revision_one() {
        // etcd's own starting point. A store that started at 0 would make
        // revision 1 unwatchable, and "watch from the beginning" is spelled
        // as revision 1.
        assert_eq!(store().revision().unwrap(), 1);
    }

    #[test]
    fn each_write_advances_the_revision_by_one() {
        let mut s = store();
        assert_eq!(put(&mut s, "/a", "1").revision, 2);
        assert_eq!(put(&mut s, "/b", "1").revision, 3);
        assert_eq!(put(&mut s, "/a", "2").revision, 4);
    }

    #[test]
    fn a_delete_that_matches_nothing_does_not_burn_a_revision() {
        // etcd is specific about this, and it matters: a client that saw the
        // revision move without a corresponding event would conclude it had
        // missed something and re-list.
        let mut s = store();
        put(&mut s, "/a", "1");
        let before = s.revision().unwrap();
        let applied = s
            .apply(&Command::Delete(DeleteOp {
                range: KeyRange::Single(b"/nonexistent".to_vec()),
                prev_kv: false,
            }))
            .unwrap();
        assert_eq!(applied.revision, before);
        assert_eq!(s.revision().unwrap(), before);
        assert!(applied.events.is_empty());
    }

    #[test]
    fn every_write_in_one_transaction_shares_a_revision() {
        let mut s = store();
        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![],
                success: vec![
                    RequestOp::Put(PutOp {
                        key: b"/a".to_vec(),
                        value: b"1".to_vec(),
                        lease: 0,
                        prev_kv: false,
                        ignore_value: false,
                        ignore_lease: false,
                    }),
                    RequestOp::Put(PutOp {
                        key: b"/b".to_vec(),
                        value: b"2".to_vec(),
                        lease: 0,
                        prev_kv: false,
                        ignore_value: false,
                        ignore_lease: false,
                    }),
                ],
                failure: vec![],
            }))
            .unwrap();
        assert_eq!(applied.revision, 2, "one transaction, one revision");
        assert_eq!(applied.events.len(), 2);
        assert_eq!(get(&s, "/a").unwrap().mod_revision, 2);
        assert_eq!(get(&s, "/b").unwrap().mod_revision, 2);
    }

    // ── Versions and re-creation ─────────────────────────────────────────

    #[test]
    fn version_counts_writes_and_create_revision_stays_put() {
        let mut s = store();
        put(&mut s, "/a", "1");
        put(&mut s, "/a", "2");
        let kv = get(&s, "/a").unwrap();
        assert_eq!(kv.create_revision, 2, "still the revision it was created at");
        assert_eq!(kv.mod_revision, 3);
        assert_eq!(kv.version, 2);
    }

    #[test]
    fn recreating_a_deleted_key_resets_version_and_create_revision() {
        // A re-created key is a *different* object to apiserver's optimistic
        // concurrency. If version kept climbing across the deletion, a stale
        // client's compare-and-swap could succeed against the new object.
        let mut s = store();
        put(&mut s, "/a", "1");
        s.apply(&Command::Delete(DeleteOp { range: KeyRange::Single(b"/a".to_vec()), prev_kv: false }))
            .unwrap();
        put(&mut s, "/a", "2");
        let kv = get(&s, "/a").unwrap();
        assert_eq!(kv.version, 1);
        assert_eq!(kv.create_revision, 4);
    }

    #[test]
    fn a_deleted_key_is_gone_from_reads() {
        let mut s = store();
        put(&mut s, "/a", "1");
        let applied = s
            .apply(&Command::Delete(DeleteOp {
                range: KeyRange::Single(b"/a".to_vec()),
                prev_kv: true,
            }))
            .unwrap();
        match applied.response {
            CommandResponse::Delete { deleted, prev_kvs } => {
                assert_eq!(deleted, 1);
                assert_eq!(prev_kvs[0].value, b"1");
            }
            other => panic!("expected a delete response, got {other:?}"),
        }
        assert!(get(&s, "/a").is_none());
    }

    // ── Transactions ─────────────────────────────────────────────────────

    #[test]
    fn compare_against_a_missing_key_sees_zeroes() {
        // This is how "create only if absent" is spelled, and every object
        // apiserver creates depends on it: a missing key must compare equal
        // to mod_revision 0, not fail for lack of a row.
        let mut s = store();
        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![Compare {
                    key: b"/a".to_vec(),
                    result: CompareResult::Equal,
                    target: CompareTarget::ModRevision(0),
                }],
                success: vec![RequestOp::Put(PutOp {
                    key: b"/a".to_vec(),
                    value: b"created".to_vec(),
                    lease: 0,
                    prev_kv: false,
                    ignore_value: false,
                    ignore_lease: false,
                })],
                failure: vec![],
            }))
            .unwrap();
        match applied.response {
            CommandResponse::Txn { succeeded, .. } => assert!(succeeded),
            other => panic!("expected a txn response, got {other:?}"),
        }
        assert_eq!(get(&s, "/a").unwrap().value, b"created");
    }

    #[test]
    fn a_stale_compare_and_swap_loses_and_reads_back_the_winner() {
        // The exact shape apiserver uses on a conflicting update: compare on
        // the resourceVersion it read, and take a Range in the failure branch
        // to learn what actually won.
        let mut s = store();
        put(&mut s, "/a", "v1");
        let stale_rev = get(&s, "/a").unwrap().mod_revision;
        put(&mut s, "/a", "v2");

        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![Compare {
                    key: b"/a".to_vec(),
                    result: CompareResult::Equal,
                    target: CompareTarget::ModRevision(stale_rev),
                }],
                success: vec![RequestOp::Put(PutOp {
                    key: b"/a".to_vec(),
                    value: b"v3-should-not-win".to_vec(),
                    lease: 0,
                    prev_kv: false,
                    ignore_value: false,
                    ignore_lease: false,
                })],
                failure: vec![RequestOp::Range(RangeQuery::current(KeyRange::Single(
                    b"/a".to_vec(),
                )))],
            }))
            .unwrap();

        match applied.response {
            CommandResponse::Txn { succeeded, responses } => {
                assert!(!succeeded, "a stale compare must not win");
                match &responses[0] {
                    OpResponse::Range(r) => assert_eq!(r.kvs[0].value, b"v2"),
                    other => panic!("expected the failure branch's range, got {other:?}"),
                }
            }
            other => panic!("expected a txn response, got {other:?}"),
        }
        assert_eq!(get(&s, "/a").unwrap().value, b"v2", "the losing put must not have applied");
    }

    #[test]
    fn a_transaction_whose_branch_is_empty_burns_no_revision() {
        let mut s = store();
        put(&mut s, "/a", "1");
        let before = s.revision().unwrap();
        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![Compare {
                    key: b"/a".to_vec(),
                    result: CompareResult::Equal,
                    target: CompareTarget::ModRevision(999),
                }],
                success: vec![],
                failure: vec![],
            }))
            .unwrap();
        assert_eq!(applied.revision, before);
    }

    #[test]
    fn a_transaction_reads_its_own_writes() {
        let mut s = store();
        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![],
                success: vec![
                    RequestOp::Put(PutOp {
                        key: b"/a".to_vec(),
                        value: b"written".to_vec(),
                        lease: 0,
                        prev_kv: false,
                        ignore_value: false,
                        ignore_lease: false,
                    }),
                    RequestOp::Range(RangeQuery::current(KeyRange::Single(b"/a".to_vec()))),
                ],
                failure: vec![],
            }))
            .unwrap();
        match applied.response {
            CommandResponse::Txn { responses, .. } => match &responses[1] {
                OpResponse::Range(r) => assert_eq!(r.kvs[0].value, b"written"),
                other => panic!("expected a range, got {other:?}"),
            },
            other => panic!("expected a txn response, got {other:?}"),
        }
    }

    #[test]
    fn a_transaction_is_atomic_across_its_deletes() {
        // Two deletes in one txn must report their own counts, not each
        // other's — they share a revision, which makes "count the events at
        // this revision" the wrong way to answer.
        let mut s = store();
        put(&mut s, "/a/1", "x");
        put(&mut s, "/a/2", "x");
        put(&mut s, "/b/1", "x");
        let applied = s
            .apply(&Command::Txn(TxnOp {
                compare: vec![],
                success: vec![
                    RequestOp::Delete(DeleteOp {
                        range: KeyRange::Between { from: b"/a/".to_vec(), to: b"/a0".to_vec() },
                        prev_kv: false,
                    }),
                    RequestOp::Delete(DeleteOp {
                        range: KeyRange::Single(b"/b/1".to_vec()),
                        prev_kv: false,
                    }),
                ],
                failure: vec![],
            }))
            .unwrap();
        match applied.response {
            CommandResponse::Txn { responses, .. } => {
                match &responses[0] {
                    OpResponse::Delete { deleted, .. } => assert_eq!(*deleted, 2),
                    other => panic!("expected a delete, got {other:?}"),
                }
                match &responses[1] {
                    OpResponse::Delete { deleted, .. } => assert_eq!(*deleted, 1),
                    other => panic!("expected a delete, got {other:?}"),
                }
            }
            other => panic!("expected a txn response, got {other:?}"),
        }
    }

    // ── Ranges ───────────────────────────────────────────────────────────

    #[test]
    fn a_prefix_range_returns_keys_in_order() {
        let mut s = store();
        put(&mut s, "/registry/pods/b", "2");
        put(&mut s, "/registry/pods/a", "1");
        put(&mut s, "/registry/nodes/n", "n");
        let r = s
            .range(&RangeQuery::current(KeyRange::Between {
                from: b"/registry/pods/".to_vec(),
                to: b"/registry/pods0".to_vec(),
            }))
            .unwrap();
        let keys: Vec<_> = r.kvs.iter().map(|kv| kv.key.clone()).collect();
        assert_eq!(keys, vec![b"/registry/pods/a".to_vec(), b"/registry/pods/b".to_vec()]);
        assert_eq!(r.count, 2);
    }

    #[test]
    fn limit_truncates_and_reports_more_without_lying_about_count() {
        // apiserver turns `more` into a continue token; `count` has to stay
        // the full total or paging reports the wrong remaining size.
        let mut s = store();
        for k in ["/a", "/b", "/c"] {
            put(&mut s, k, "x");
        }
        let mut q = RangeQuery::current(KeyRange::All);
        q.limit = 2;
        let r = s.range(&q).unwrap();
        assert_eq!(r.kvs.len(), 2);
        assert!(r.more);
        assert_eq!(r.count, 3);
    }

    #[test]
    fn limit_equal_to_the_result_size_reports_no_more() {
        let mut s = store();
        for k in ["/a", "/b"] {
            put(&mut s, k, "x");
        }
        let mut q = RangeQuery::current(KeyRange::All);
        q.limit = 2;
        let r = s.range(&q).unwrap();
        assert_eq!(r.kvs.len(), 2);
        assert!(!r.more, "exactly-full page is not a truncated one");
    }

    #[test]
    fn a_historical_read_sees_the_store_as_it_was() {
        let mut s = store();
        put(&mut s, "/a", "v1");
        let at = s.revision().unwrap();
        put(&mut s, "/a", "v2");
        put(&mut s, "/b", "new");

        let mut q = RangeQuery::current(KeyRange::All);
        q.revision = at;
        let r = s.range(&q).unwrap();
        assert_eq!(r.kvs.len(), 1, "/b did not exist yet");
        assert_eq!(r.kvs[0].value, b"v1");
    }

    #[test]
    fn a_historical_read_of_a_since_deleted_key_still_sees_it() {
        let mut s = store();
        put(&mut s, "/a", "v1");
        let at = s.revision().unwrap();
        s.apply(&Command::Delete(DeleteOp { range: KeyRange::Single(b"/a".to_vec()), prev_kv: false }))
            .unwrap();
        let mut q = RangeQuery::current(KeyRange::Single(b"/a".to_vec()));
        q.revision = at;
        assert_eq!(s.range(&q).unwrap().kvs[0].value, b"v1");
    }

    #[test]
    fn count_only_skips_the_rows() {
        let mut s = store();
        for k in ["/a", "/b"] {
            put(&mut s, k, "x");
        }
        let mut q = RangeQuery::current(KeyRange::All);
        q.count_only = true;
        let r = s.range(&q).unwrap();
        assert_eq!(r.count, 2);
        assert!(r.kvs.is_empty());
    }

    #[test]
    fn keys_only_omits_values() {
        let mut s = store();
        put(&mut s, "/a", "secret");
        let mut q = RangeQuery::current(KeyRange::All);
        q.keys_only = true;
        let r = s.range(&q).unwrap();
        assert_eq!(r.kvs[0].key, b"/a");
        assert!(r.kvs[0].value.is_empty());
        assert_eq!(r.kvs[0].mod_revision, 2, "metadata still comes back");
    }

    #[test]
    fn a_future_revision_read_is_refused() {
        let s = store();
        let mut q = RangeQuery::current(KeyRange::All);
        q.revision = 99;
        assert!(matches!(s.range(&q), Err(Error::FutureRevision { .. })));
    }

    // ── Compaction ───────────────────────────────────────────────────────

    #[test]
    fn compaction_drops_history_but_keeps_live_keys_readable() {
        let mut s = store();
        put(&mut s, "/a", "v1");
        put(&mut s, "/a", "v2");
        let at = s.revision().unwrap();
        s.apply(&Command::Compact { revision: at }).unwrap();

        assert_eq!(get(&s, "/a").unwrap().value, b"v2", "the live key survives compaction");
        let mut q = RangeQuery::current(KeyRange::Single(b"/a".to_vec()));
        q.revision = 2;
        assert!(matches!(s.range(&q), Err(Error::Compacted { .. })));
    }

    #[test]
    fn compacting_backwards_is_refused() {
        let mut s = store();
        put(&mut s, "/a", "1");
        put(&mut s, "/a", "2");
        s.apply(&Command::Compact { revision: 3 }).unwrap();
        assert!(matches!(s.apply(&Command::Compact { revision: 2 }), Err(Error::Compacted { .. })));
    }

    #[test]
    fn compaction_does_not_move_the_revision() {
        let mut s = store();
        put(&mut s, "/a", "1");
        let before = s.revision().unwrap();
        s.apply(&Command::Compact { revision: before }).unwrap();
        assert_eq!(s.revision().unwrap(), before);
    }

    // ── Watch replay ─────────────────────────────────────────────────────

    #[test]
    fn events_since_replays_in_apply_order_with_prev_values() {
        let mut s = store();
        put(&mut s, "/a", "v1");
        put(&mut s, "/a", "v2");
        s.apply(&Command::Delete(DeleteOp { range: KeyRange::Single(b"/a".to_vec()), prev_kv: false }))
            .unwrap();

        let events = s.events_since(1, &KeyRange::All).unwrap();
        assert_eq!(events.len(), 3);

        assert_eq!(events[0].1.kind, EventKind::Put);
        assert!(events[0].1.prev_kv.is_none(), "a create has no previous value");

        assert_eq!(events[1].1.kind, EventKind::Put);
        assert_eq!(events[1].1.prev_kv.as_ref().unwrap().value, b"v1");

        // The one that matters most: apiserver's watch cache builds the
        // deleted object out of prev_kv. Without it, every DELETE delivers an
        // empty object and downstream controllers see a nameless deletion.
        assert_eq!(events[2].1.kind, EventKind::Delete);
        assert_eq!(events[2].1.prev_kv.as_ref().unwrap().value, b"v2");
        assert!(events[2].1.kv.value.is_empty(), "a delete event carries no value of its own");
    }

    #[test]
    fn events_since_is_filtered_by_range() {
        let mut s = store();
        put(&mut s, "/a/1", "x");
        put(&mut s, "/b/1", "x");
        let events = s
            .events_since(1, &KeyRange::Between { from: b"/a/".to_vec(), to: b"/a0".to_vec() })
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1.kv.key, b"/a/1");
    }

    #[test]
    fn replaying_from_before_the_compaction_point_is_refused() {
        // The honest answer is "I cannot tell you what you missed", which is
        // what makes apiserver re-list instead of silently skipping events.
        let mut s = store();
        put(&mut s, "/a", "v1");
        put(&mut s, "/a", "v2");
        s.apply(&Command::Compact { revision: 3 }).unwrap();
        assert!(matches!(s.events_since(1, &KeyRange::All), Err(Error::Compacted { .. })));
    }

    // ── Leases ───────────────────────────────────────────────────────────

    #[test]
    fn a_revoked_lease_deletes_the_keys_it_held() {
        let mut s = store();
        s.apply(&Command::LeaseGrant { id: 7, ttl_secs: 60, now_unix_secs: 0 }).unwrap();
        s.apply(&Command::Put(PutOp {
            key: b"/e/1".to_vec(),
            value: b"x".to_vec(),
            lease: 7,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        }))
        .unwrap();
        put(&mut s, "/keep", "x");

        let applied = s.apply(&Command::LeaseRevoke { id: 7 }).unwrap();
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].kind, EventKind::Delete);
        assert!(get(&s, "/e/1").is_none());
        assert!(get(&s, "/keep").is_some(), "only the leased key goes");
        assert!(s.lease_ttl(7).unwrap().is_none());
    }

    #[test]
    fn expiry_uses_the_timestamp_in_the_command_not_the_local_clock() {
        // The determinism rule from command.rs, tested where it is easy to
        // break: apply() must never consult a clock, or two replicas expiring
        // the same lease at different wall times would diverge.
        let mut s = store();
        s.apply(&Command::LeaseGrant { id: 1, ttl_secs: 10, now_unix_secs: 0 }).unwrap();
        s.apply(&Command::LeaseKeepAlive { id: 1, now_unix_secs: 1_000 }).unwrap();
        s.apply(&Command::Put(PutOp {
            key: b"/e".to_vec(),
            value: b"x".to_vec(),
            lease: 1,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        }))
        .unwrap();

        // 1005 is inside the 10s TTL that started at 1000.
        let applied = s.apply(&Command::ExpireLeases { now_unix_secs: 1_005 }).unwrap();
        assert!(applied.events.is_empty(), "not expired yet");
        assert!(get(&s, "/e").is_some());

        let applied = s.apply(&Command::ExpireLeases { now_unix_secs: 1_011 }).unwrap();
        assert_eq!(applied.events.len(), 1);
        assert!(get(&s, "/e").is_none());
    }

    #[test]
    fn a_keepalive_for_an_unknown_lease_answers_zero_rather_than_failing() {
        let mut s = store();
        let applied = s.apply(&Command::LeaseKeepAlive { id: 404, now_unix_secs: 1 }).unwrap();
        match applied.response {
            CommandResponse::Lease { ttl_secs } => assert_eq!(ttl_secs, 0),
            other => panic!("expected a lease response, got {other:?}"),
        }
    }

    #[test]
    fn ignore_value_keeps_the_value_and_changes_only_the_lease() {
        let mut s = store();
        s.apply(&Command::LeaseGrant { id: 3, ttl_secs: 60, now_unix_secs: 0 }).unwrap();
        put(&mut s, "/a", "keepme");
        s.apply(&Command::Put(PutOp {
            key: b"/a".to_vec(),
            value: Vec::new(),
            lease: 3,
            prev_kv: false,
            ignore_value: true,
            ignore_lease: false,
        }))
        .unwrap();
        let kv = get(&s, "/a").unwrap();
        assert_eq!(kv.value, b"keepme");
        assert_eq!(kv.lease, 3);
    }

    #[test]
    fn ignore_value_on_a_missing_key_is_an_error() {
        let mut s = store();
        let r = s.apply(&Command::Put(PutOp {
            key: b"/nope".to_vec(),
            value: Vec::new(),
            lease: 0,
            prev_kv: false,
            ignore_value: true,
            ignore_lease: false,
        }));
        assert!(matches!(r, Err(Error::KeyNotFound)));
    }

    // ── Durability ───────────────────────────────────────────────────────

    #[test]
    fn state_survives_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut s = Store::open(&path).unwrap();
            put(&mut s, "/a", "v1");
            put(&mut s, "/a", "v2");
            s.apply(&Command::Compact { revision: 2 }).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.revision().unwrap(), 3);
        assert_eq!(s.compact_revision().unwrap(), 2);
        assert_eq!(get(&s, "/a").unwrap().value, b"v2");
    }
}

#[cfg(test)]
mod member_tests {
    use super::*;
    use crate::command::Member;

    fn member(id: u64, learner: bool) -> Member {
        Member {
            id,
            peer_url: format!("http://10.0.0.{id}:2380"),
            client_url: format!("http://10.0.0.{id}:2379"),
            name: format!("node-{id}"),
            is_learner: learner,
        }
    }

    #[test]
    fn members_round_trip_and_can_be_looked_up_by_id() {
        // The lookup is what a follower uses to turn "the leader is id 3"
        // into a URL it can forward a write to.
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply(&Command::SetMember(member(1, false))).unwrap();
        s.apply(&Command::SetMember(member(2, true))).unwrap();

        assert_eq!(s.members().unwrap().len(), 2);
        let m = s.member(2).unwrap().expect("member 2");
        assert_eq!(m.client_url, "http://10.0.0.2:2379");
        assert!(m.is_learner);
        assert!(s.member(99).unwrap().is_none());
    }

    #[test]
    fn re_setting_a_member_updates_it_rather_than_duplicating() {
        // A member that changed address must not end up listed twice, or a
        // follower could forward to the address it used to have.
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply(&Command::SetMember(member(1, true))).unwrap();
        let mut promoted = member(1, false);
        promoted.client_url = "http://10.0.0.99:2379".to_string();
        s.apply(&Command::SetMember(promoted)).unwrap();

        let members = s.members().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].client_url, "http://10.0.0.99:2379");
        assert!(!members[0].is_learner, "promotion must stick");
    }

    #[test]
    fn removing_a_member_forgets_it() {
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply(&Command::SetMember(member(1, false))).unwrap();
        s.apply(&Command::SetMember(member(2, false))).unwrap();
        s.apply(&Command::RemoveMember { id: 1 }).unwrap();

        let members = s.members().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, 2);
    }

    #[test]
    fn membership_changes_do_not_advance_the_store_revision() {
        // Membership is cluster metadata, not user data. A watching client
        // seeing the revision move for a member change would conclude it had
        // missed an event under /registry and re-list for nothing.
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply(&Command::Put(PutOp {
            key: b"/a".to_vec(),
            value: b"1".to_vec(),
            lease: 0,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        }))
        .unwrap();
        let before = s.revision().unwrap();

        let applied = s.apply(&Command::SetMember(member(3, false))).unwrap();
        assert_eq!(applied.revision, before);
        assert!(applied.events.is_empty(), "a member change is not a kv event");

        let applied = s.apply(&Command::RemoveMember { id: 3 }).unwrap();
        assert_eq!(applied.revision, before);
    }

    #[test]
    fn the_address_book_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.apply(&Command::SetMember(member(1, false))).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.members().unwrap()[0].peer_url, "http://10.0.0.1:2380");
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::command::Member;

    fn put(s: &mut Store, key: &str, value: &str) {
        s.apply(&Command::Put(PutOp {
            key: key.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
            lease: 0,
            prev_kv: false,
            ignore_value: false,
            ignore_lease: false,
        }))
        .unwrap();
    }

    #[test]
    fn a_snapshot_restores_into_an_identical_store() {
        // The property that matters: a follower restored from a snapshot must
        // answer reads the same way as the member that sent it, including
        // revisions — a resourceVersion that changed across a restore would
        // break every client's optimistic concurrency at once.
        let mut source = Store::open(Path::new(":memory:")).unwrap();
        put(&mut source, "/a", "1");
        put(&mut source, "/b", "2");
        put(&mut source, "/a", "3");
        source
            .apply(&Command::Delete(DeleteOp {
                range: KeyRange::Single(b"/b".to_vec()),
                prev_kv: false,
            }))
            .unwrap();
        source.apply(&Command::LeaseGrant { id: 9, ttl_secs: 30, now_unix_secs: 100 }).unwrap();
        source
            .apply(&Command::SetMember(Member {
                id: 2,
                peer_url: "http://p".into(),
                client_url: "http://c".into(),
                name: "n2".into(),
                is_learner: false,
            }))
            .unwrap();

        let snapshot = source.export_snapshot().unwrap();

        let mut target = Store::open(Path::new(":memory:")).unwrap();
        put(&mut target, "/should-be-erased", "x");
        target.restore_snapshot(&snapshot).unwrap();

        assert_eq!(target.revision().unwrap(), source.revision().unwrap());
        let a = target
            .range(&RangeQuery::current(KeyRange::Single(b"/a".to_vec())))
            .unwrap()
            .kvs
            .remove(0);
        assert_eq!(a.value, b"3");
        assert_eq!(a.mod_revision, 4, "revisions must survive the restore unchanged");
        assert!(
            target.range(&RangeQuery::current(KeyRange::Single(b"/b".to_vec()))).unwrap().kvs.is_empty(),
            "a key deleted before the snapshot must not come back"
        );
        assert_eq!(target.lease_ttl(9).unwrap().map(|(ttl, _)| ttl), Some(30));
        assert_eq!(target.members().unwrap()[0].id, 2);
    }

    #[test]
    fn restoring_erases_whatever_was_there_before() {
        // A follower receiving a snapshot is one whose own state is known to
        // be unreconstructable, so merging would preserve exactly the rows
        // that must not survive.
        let mut source = Store::open(Path::new(":memory:")).unwrap();
        put(&mut source, "/keep", "yes");
        let snapshot = source.export_snapshot().unwrap();

        let mut target = Store::open(Path::new(":memory:")).unwrap();
        put(&mut target, "/stale", "must not survive");
        target.restore_snapshot(&snapshot).unwrap();

        assert!(target
            .range(&RangeQuery::current(KeyRange::Single(b"/stale".to_vec())))
            .unwrap()
            .kvs
            .is_empty());
        assert_eq!(
            target.range(&RangeQuery::current(KeyRange::All)).unwrap().kvs.len(),
            1,
            "only the snapshot's keys"
        );
    }

    #[test]
    fn a_snapshot_of_a_multi_key_transaction_restores() {
        // The regression test for restore hardcoding sub = 0.
        //
        // One transaction writes several keys at the *same* revision with
        // different subs, so the snapshot has repeated mod_revisions. With
        // PRIMARY KEY (revision, sub), the second row at sub 0 failed the
        // UNIQUE constraint and rolled the entire restore back — leaving a
        // follower past the compaction point with no recovery path at all.
        //
        // The other snapshot tests use single-key puts at distinct revisions,
        // which structurally cannot reach this.
        let mut source = Store::open(Path::new(":memory:")).unwrap();
        source
            .apply(&Command::Txn(TxnOp {
                compare: vec![],
                success: vec!["/a", "/b", "/c"]
                    .into_iter()
                    .map(|k| {
                        RequestOp::Put(PutOp {
                            key: k.as_bytes().to_vec(),
                            value: b"same-revision".to_vec(),
                            lease: 0,
                            prev_kv: false,
                            ignore_value: false,
                            ignore_lease: false,
                        })
                    })
                    .collect(),
                failure: vec![],
            }))
            .unwrap();

        let snapshot = source.export_snapshot().unwrap();
        let shared: Vec<i64> = snapshot.kvs.iter().map(|kv| kv.mod_revision).collect();
        assert_eq!(shared, vec![2, 2, 2], "the three keys must share one revision");

        let mut target = Store::open(Path::new(":memory:")).unwrap();
        target.restore_snapshot(&snapshot).expect("a multi-key revision must restore");

        let all = target.range(&RangeQuery::current(KeyRange::All)).unwrap();
        assert_eq!(all.kvs.len(), 3, "every key in the shared revision must survive");
        for kv in &all.kvs {
            assert_eq!(kv.value, b"same-revision");
            assert_eq!(kv.mod_revision, 2);
        }
    }

    #[test]
    fn a_snapshot_of_a_revoked_lease_restores() {
        // The same shape from the other direction: revoking a lease deletes
        // every key it held at one revision.
        let mut source = Store::open(Path::new(":memory:")).unwrap();
        source.apply(&Command::LeaseGrant { id: 4, ttl_secs: 60, now_unix_secs: 0 }).unwrap();
        for k in ["/l1", "/l2"] {
            source
                .apply(&Command::Put(PutOp {
                    key: k.as_bytes().to_vec(),
                    value: b"leased".to_vec(),
                    lease: 4,
                    prev_kv: false,
                    ignore_value: false,
                    ignore_lease: false,
                }))
                .unwrap();
        }
        put(&mut source, "/keep", "kept");
        source.apply(&Command::LeaseRevoke { id: 4 }).unwrap();

        let snapshot = source.export_snapshot().unwrap();
        let mut target = Store::open(Path::new(":memory:")).unwrap();
        target.restore_snapshot(&snapshot).expect("restore after a lease revocation");
        assert_eq!(target.range(&RangeQuery::current(KeyRange::All)).unwrap().kvs.len(), 1);
    }

    #[test]
    fn the_applied_index_moves_with_the_state_it_describes() {
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        assert_eq!(s.applied_index().unwrap(), 0);
        s.apply_at(
            17,
            &Command::Put(PutOp {
                key: b"/a".to_vec(),
                value: b"1".to_vec(),
                lease: 0,
                prev_kv: false,
                ignore_value: false,
                ignore_lease: false,
            }),
        )
        .unwrap();
        assert_eq!(s.applied_index().unwrap(), 17);
    }

    #[test]
    fn a_command_that_writes_nothing_still_advances_the_applied_index() {
        // Otherwise a replica would re-apply it forever after a restart:
        // the entry is committed and consumed whether or not it changed
        // anything.
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply_at(
            5,
            &Command::Delete(DeleteOp { range: KeyRange::Single(b"/nope".to_vec()), prev_kv: false }),
        )
        .unwrap();
        assert_eq!(s.applied_index().unwrap(), 5);
        assert_eq!(s.revision().unwrap(), 1, "and it still burns no revision");
    }

    #[test]
    fn the_applied_index_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.apply_at(42, &Command::Compact { revision: 1 }).unwrap();
        }
        assert_eq!(Store::open(&path).unwrap().applied_index().unwrap(), 42);
    }

    #[test]
    fn a_snapshot_carries_the_applied_index_it_was_taken_at() {
        // A restored follower must resume from the snapshot's index, not
        // from zero, or it will re-request the entire log.
        let mut s = Store::open(Path::new(":memory:")).unwrap();
        s.apply_at(
            11,
            &Command::Put(PutOp {
                key: b"/a".to_vec(),
                value: b"1".to_vec(),
                lease: 0,
                prev_kv: false,
                ignore_value: false,
                ignore_lease: false,
            }),
        )
        .unwrap();
        let snapshot = s.export_snapshot().unwrap();
        assert_eq!(snapshot.applied_index, 11);

        let mut target = Store::open(Path::new(":memory:")).unwrap();
        target.restore_snapshot(&snapshot).unwrap();
        assert_eq!(target.applied_index().unwrap(), 11);
    }
}
