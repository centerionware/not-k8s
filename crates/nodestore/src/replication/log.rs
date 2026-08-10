//! The raft log, in sqlite: an implementation of `raft::Storage`.
//!
//! # Two databases, two durability settings, and why that is not an
//! optimisation
//!
//! The raft log lives in its own database file with `synchronous = FULL`,
//! while the state machine's stays on `NORMAL`. That asymmetry is required,
//! not tuned:
//!
//!   * Raft's correctness rests on a promise that an entry a member has
//!     acknowledged is *durable*. A quorum acknowledging entries that a power
//!     cut then erases is exactly how a committed write is lost — the one
//!     thing consensus exists to prevent. So the log fsyncs.
//!   * The state machine has no such obligation, because it is *derivable*:
//!     it is nothing but the log applied in order. If it loses its tail, the
//!     missing entries are replayed on startup.
//!
//! That second point only holds because the applied index is written in the
//! **same sqlite transaction** as the state it produced (see
//! [`crate::store::Store::apply`]). Losing the tail of the state database
//! therefore rolls back the applied index *and* the state together, leaving a
//! consistent — merely older — replica that the log can catch up. Had the
//! applied index been stored separately, a crash between the two would leave
//! a replica that believes it applied an entry it did not, and no amount of
//! replaying fixes that: it is silent divergence from the rest of the
//! cluster.
//!
//! # Compaction and snapshots
//!
//! A log that only grows is a disk that eventually fills, so old entries are
//! discarded once they are safely applied and captured in a snapshot. The
//! moment that happens, a follower that is further behind than the compaction
//! point can no longer be caught up by sending it entries — they no longer
//! exist. That follower needs the snapshot instead. This is why snapshots are
//! not a "nice to have later": compaction without them turns a temporarily
//! slow follower into a permanently broken one.

use crate::error::Error as StoreError;
use raft::eraftpb::{ConfState, Entry, EntryType, HardState, Snapshot, SnapshotMetadata};
use raft::{Error as RaftError, GetEntriesContext, RaftState, Storage, StorageError};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Shared handle to the raft log. Cloneable because `raft::RawNode` owns one
/// and the driver needs another to append to it.
#[derive(Clone)]
pub struct RaftLog {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    /// The most recent snapshot this node can serve, if any.
    ///
    /// Cached in memory rather than re-read per request because raft asks for
    /// it while holding its own loop, and because building one walks the
    /// entire state machine.
    snapshot: Option<Snapshot>,
    /// Set when raft has asked for a snapshot we do not have yet, so the
    /// driver knows to build one. Raft is told "temporarily unavailable" in
    /// the meantime, which it is built to retry.
    snapshot_requested: Option<u64>,
}

fn store_err(e: impl std::fmt::Display) -> RaftError {
    RaftError::Store(StorageError::Other(Box::new(std::io::Error::other(e.to_string()))))
}

impl RaftLog {
    pub fn open(path: &Path) -> Result<RaftLog, StoreError> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        // synchronous = FULL: see the module header. This is the fsync raft's
        // durability promise is made of, and turning it down would trade
        // correctness for write throughput on the one component that must not
        // make that trade.
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;

            CREATE TABLE IF NOT EXISTS entries (
                idx        INTEGER PRIMARY KEY,
                term       INTEGER NOT NULL,
                entry_type INTEGER NOT NULL,
                data       BLOB,
                context    BLOB
            );

            CREATE TABLE IF NOT EXISTS state (
                name  TEXT PRIMARY KEY,
                value BLOB NOT NULL
            );

            -- Metadata of the snapshot the log has been compacted up to.
            -- Separate from `state` because it is written together with a
            -- compaction, not with raft's own persisted state.
            CREATE TABLE IF NOT EXISTS snapshot_meta (
                id         INTEGER PRIMARY KEY CHECK (id = 1),
                idx        INTEGER NOT NULL,
                term       INTEGER NOT NULL,
                conf_state BLOB NOT NULL
            );
            "#,
        )?;
        Ok(RaftLog {
            inner: Arc::new(Mutex::new(Inner { conn, snapshot: None, snapshot_requested: None })),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, RaftError> {
        self.inner.lock().map_err(|_| store_err("raft log mutex poisoned"))
    }

    // ── Persisted raft state ─────────────────────────────────────────────

    pub fn set_hard_state(&self, hs: &HardState) -> Result<(), StoreError> {
        self.put_state("hard_state", &encode_pb(hs)?)
    }

    pub fn set_conf_state(&self, cs: &ConfState) -> Result<(), StoreError> {
        self.put_state("conf_state", &encode_pb(cs)?)
    }

    fn put_state(&self, name: &str, value: &[u8]) -> Result<(), StoreError> {
        let inner = self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
        inner.conn.execute(
            "INSERT INTO state (name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            rusqlite::params![name, value],
        )?;
        Ok(())
    }

    fn get_state(conn: &Connection, name: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
        conn.query_row("SELECT value FROM state WHERE name = ?1", [name], |r| r.get(0)).optional()
    }

    // ── Log mutation ─────────────────────────────────────────────────────

    /// Append entries, truncating any conflicting suffix first.
    ///
    /// The truncation is not defensive tidying: a follower whose log diverged
    /// from the leader's is told to overwrite from the divergence point, and
    /// leaving the old suffix in place would resurrect entries the cluster
    /// has decided never happened.
    pub fn append(&self, entries: &[Entry]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
        let tx = inner.conn.transaction()?;
        let first = entries[0].index;
        tx.execute("DELETE FROM entries WHERE idx >= ?1", [first])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO entries (idx, term, entry_type, data, context)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in entries {
                // .value() rather than a cast: EntryType is a rust-protobuf
                // enum, and going through the trait keeps this correct if a
                // future raft-rs represents it differently.
                stmt.execute(rusqlite::params![
                    e.index,
                    e.term,
                    protobuf::ProtobufEnum::value(&e.entry_type),
                    e.data.as_ref(),
                    e.context.as_ref(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Discard entries at or below `index`, which must already be covered by
    /// a snapshot.
    pub fn compact_to(&self, index: u64) -> Result<(), StoreError> {
        let inner = self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
        inner.conn.execute("DELETE FROM entries WHERE idx <= ?1", [index])?;
        Ok(())
    }

    /// Record a snapshot this node can serve, and the compaction point it
    /// establishes.
    pub fn install_snapshot(&self, snapshot: Snapshot) -> Result<(), StoreError> {
        let meta = snapshot.get_metadata().clone();
        let conf_state = encode_pb(meta.get_conf_state())?;
        {
            let mut inner =
                self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
            let tx = inner.conn.transaction()?;
            tx.execute(
                "INSERT INTO snapshot_meta (id, idx, term, conf_state) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET idx = excluded.idx, term = excluded.term,
                                               conf_state = excluded.conf_state",
                rusqlite::params![meta.index, meta.term, conf_state],
            )?;
            // Entries the snapshot subsumes are dead weight, and keeping them
            // would let first_index disagree with the snapshot.
            tx.execute("DELETE FROM entries WHERE idx <= ?1", [meta.index])?;
            tx.commit()?;
            inner.snapshot = Some(snapshot);
            inner.snapshot_requested = None;
        }
        Ok(())
    }

    /// The index raft has asked for a snapshot at, if it is still waiting.
    pub fn pending_snapshot_request(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|i| i.snapshot_requested)
    }

    pub fn snapshot_index(&self) -> Result<u64, StoreError> {
        let inner = self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
        Ok(snapshot_meta(&inner.conn)?.map(|(idx, _, _)| idx).unwrap_or(0))
    }

    pub fn last_index_value(&self) -> Result<u64, StoreError> {
        let inner = self.inner.lock().map_err(|_| StoreError::Unavailable("raft log".into()))?;
        last_index_of(&inner.conn).map_err(StoreError::from)
    }
}

fn snapshot_meta(conn: &Connection) -> Result<Option<(u64, u64, Vec<u8>)>, rusqlite::Error> {
    conn.query_row("SELECT idx, term, conf_state FROM snapshot_meta WHERE id = 1", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .optional()
}

fn last_index_of(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let from_entries: Option<u64> =
        conn.query_row("SELECT MAX(idx) FROM entries", [], |r| r.get(0)).optional()?.flatten();
    if let Some(idx) = from_entries {
        return Ok(idx);
    }
    // An empty log is not index 0 if a snapshot has been applied — the
    // snapshot's index is the last thing this node knows about.
    Ok(snapshot_meta(conn)?.map(|(idx, _, _)| idx).unwrap_or(0))
}

fn first_index_of(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let from_entries: Option<u64> =
        conn.query_row("SELECT MIN(idx) FROM entries", [], |r| r.get(0)).optional()?.flatten();
    if let Some(idx) = from_entries {
        return Ok(idx);
    }
    Ok(snapshot_meta(conn)?.map(|(idx, _, _)| idx + 1).unwrap_or(1))
}

fn encode_pb<M: protobuf::Message>(m: &M) -> Result<Vec<u8>, StoreError> {
    m.write_to_bytes()
        .map_err(|e| StoreError::InvalidRequest(format!("encoding raft state: {e}")))
}

fn decode_pb<M: protobuf::Message>(bytes: &[u8]) -> Result<M, StoreError> {
    protobuf::Message::parse_from_bytes(bytes)
        .map_err(|e| StoreError::InvalidRequest(format!("decoding raft state: {e}")))
}

impl Storage for RaftLog {
    fn initial_state(&self) -> Result<RaftState, RaftError> {
        let inner = self.lock()?;
        let hard_state = match RaftLog::get_state(&inner.conn, "hard_state").map_err(store_err)? {
            Some(bytes) => decode_pb::<HardState>(&bytes).map_err(store_err)?,
            None => HardState::default(),
        };
        // The conf state in a snapshot wins over a separately stored one: a
        // node that restored a snapshot has the membership as of that
        // snapshot, which is by definition newer than anything it recorded
        // before taking it.
        let conf_state = match snapshot_meta(&inner.conn).map_err(store_err)? {
            Some((_, _, cs)) => decode_pb::<ConfState>(&cs).map_err(store_err)?,
            None => match RaftLog::get_state(&inner.conn, "conf_state").map_err(store_err)? {
                Some(bytes) => decode_pb::<ConfState>(&bytes).map_err(store_err)?,
                None => ConfState::default(),
            },
        };
        Ok(RaftState { hard_state, conf_state })
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> Result<Vec<Entry>, RaftError> {
        let inner = self.lock()?;
        let first = first_index_of(&inner.conn).map_err(store_err)?;
        if low < first {
            // Asked for entries that have been compacted away. This is raft's
            // signal to send a snapshot instead, so it must be reported
            // precisely rather than as a generic error.
            return Err(RaftError::Store(StorageError::Compacted));
        }
        let last = last_index_of(&inner.conn).map_err(store_err)?;
        if high > last + 1 {
            return Err(RaftError::Store(StorageError::Unavailable));
        }

        let mut stmt = inner
            .conn
            .prepare("SELECT idx, term, entry_type, data, context FROM entries
                      WHERE idx >= ?1 AND idx < ?2 ORDER BY idx ASC")
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![low, high], |row| {
                let mut e = Entry::default();
                e.index = row.get(0)?;
                e.term = row.get(1)?;
                let raw_type: i32 = row.get(2)?;
                e.entry_type = protobuf::ProtobufEnum::from_i32(raw_type)
                    .unwrap_or(EntryType::EntryNormal);
                e.data = row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default().into();
                e.context = row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default().into();
                Ok(e)
            })
            .map_err(store_err)?;

        let max_size = max_size.into();
        let mut out: Vec<Entry> = Vec::new();
        let mut bytes: u64 = 0;
        for row in rows {
            let e = row.map_err(store_err)?;
            let size = protobuf::Message::compute_size(&e) as u64;
            // max_size is a soft cap that must still yield at least one entry:
            // returning none because the first one is oversized would stall
            // replication permanently on a single large write.
            if let Some(limit) = max_size {
                if !out.is_empty() && bytes + size > limit {
                    break;
                }
            }
            bytes += size;
            out.push(e);
        }
        Ok(out)
    }

    fn term(&self, idx: u64) -> Result<u64, RaftError> {
        let inner = self.lock()?;
        if let Some((snap_idx, snap_term, _)) = snapshot_meta(&inner.conn).map_err(store_err)? {
            if idx == snap_idx {
                return Ok(snap_term);
            }
            if idx < snap_idx {
                return Err(RaftError::Store(StorageError::Compacted));
            }
        }
        let term: Option<u64> = inner
            .conn
            .query_row("SELECT term FROM entries WHERE idx = ?1", [idx], |r| r.get(0))
            .optional()
            .map_err(store_err)?;
        match term {
            Some(t) => Ok(t),
            // Index 0 is the empty log's implicit predecessor, term 0.
            None if idx == 0 => Ok(0),
            None => Err(RaftError::Store(StorageError::Unavailable)),
        }
    }

    fn first_index(&self) -> Result<u64, RaftError> {
        let inner = self.lock()?;
        first_index_of(&inner.conn).map_err(store_err)
    }

    fn last_index(&self) -> Result<u64, RaftError> {
        let inner = self.lock()?;
        last_index_of(&inner.conn).map_err(store_err)
    }

    fn snapshot(&self, request_index: u64, _to: u64) -> Result<Snapshot, RaftError> {
        let mut inner = self.lock()?;
        if let Some(snapshot) = &inner.snapshot {
            if snapshot.get_metadata().index >= request_index {
                return Ok(snapshot.clone());
            }
        }
        // Building a snapshot walks the whole state machine, which must not
        // happen inside raft's own loop. Record the request; the driver builds
        // it and installs it, and raft retries — which is exactly what this
        // error means to raft, rather than being a failure.
        inner.snapshot_requested = Some(request_index);
        Err(RaftError::Store(StorageError::SnapshotTemporarilyUnavailable))
    }
}

/// Build the metadata for a snapshot at `index`/`term` with `conf_state`.
pub fn snapshot_with(index: u64, term: u64, conf_state: ConfState, data: Vec<u8>) -> Snapshot {
    let mut meta = SnapshotMetadata::default();
    meta.index = index;
    meta.term = term;
    meta.set_conf_state(conf_state);
    let mut snapshot = Snapshot::default();
    snapshot.set_metadata(meta);
    snapshot.data = data.into();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> (RaftLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let log = RaftLog::open(&dir.path().join("raft.db")).unwrap();
        (log, dir)
    }

    fn entry(index: u64, term: u64, data: &[u8]) -> Entry {
        let mut e = Entry::default();
        e.index = index;
        e.term = term;
        e.data = data.to_vec().into();
        e
    }

    #[test]
    fn an_empty_log_starts_at_index_one() {
        let (log, _d) = log();
        assert_eq!(log.first_index().unwrap(), 1);
        assert_eq!(log.last_index().unwrap(), 0);
        assert_eq!(log.term(0).unwrap(), 0);
    }

    #[test]
    fn entries_round_trip_with_their_payload() {
        let (log, _d) = log();
        log.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")]).unwrap();
        assert_eq!(log.first_index().unwrap(), 1);
        assert_eq!(log.last_index().unwrap(), 3);
        assert_eq!(log.term(3).unwrap(), 2);

        let got = log.entries(1, 4, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].data.as_ref(), b"c");
        assert_eq!(got[2].term, 2);
    }

    #[test]
    fn appending_over_a_divergent_suffix_truncates_it() {
        // The follower-diverged case. Leaving entry 3 in place would
        // resurrect an entry the cluster decided never happened.
        let (log, _d) = log();
        log.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 1, b"old")]).unwrap();
        log.append(&[entry(2, 2, b"new-2"), entry(3, 2, b"new-3")]).unwrap();

        let got = log.entries(1, 4, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].data.as_ref(), b"new-2");
        assert_eq!(got[2].data.as_ref(), b"new-3");
        assert_eq!(log.last_index().unwrap(), 3, "the old suffix must not survive");
    }

    #[test]
    fn a_shorter_append_truncates_rather_than_leaving_a_tail() {
        let (log, _d) = log();
        log.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 1, b"c")]).unwrap();
        log.append(&[entry(2, 2, b"only")]).unwrap();
        assert_eq!(log.last_index().unwrap(), 2, "entry 3 must be gone");
    }

    #[test]
    fn reading_below_the_compaction_point_reports_compacted() {
        // Raft keys on this error to decide to send a snapshot instead. A
        // generic error here would leave a lagging follower stuck forever.
        let (log, _d) = log();
        log.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 1, b"c")]).unwrap();
        log.install_snapshot(snapshot_with(2, 1, ConfState::default(), b"state".to_vec()))
            .unwrap();

        assert_eq!(log.first_index().unwrap(), 3);
        assert!(matches!(
            log.entries(1, 3, None, GetEntriesContext::empty(false)),
            Err(RaftError::Store(StorageError::Compacted))
        ));
        assert!(matches!(log.term(1), Err(RaftError::Store(StorageError::Compacted))));
        assert_eq!(log.term(2).unwrap(), 1, "the snapshot's own index still has a term");
    }

    #[test]
    fn a_snapshot_becomes_the_last_index_of_an_otherwise_empty_log() {
        // A follower that restored a snapshot and has received nothing since
        // is at the snapshot's index, not at zero.
        let (log, _d) = log();
        log.install_snapshot(snapshot_with(9, 3, ConfState::default(), b"state".to_vec()))
            .unwrap();
        assert_eq!(log.last_index().unwrap(), 9);
        assert_eq!(log.first_index().unwrap(), 10);
    }

    #[test]
    fn a_snapshot_request_ahead_of_what_we_have_is_deferred_not_failed() {
        let (log, _d) = log();
        assert!(matches!(
            log.snapshot(5, 0),
            Err(RaftError::Store(StorageError::SnapshotTemporarilyUnavailable))
        ));
        assert_eq!(log.pending_snapshot_request(), Some(5), "the driver must learn to build one");

        log.install_snapshot(snapshot_with(7, 2, ConfState::default(), b"s".to_vec())).unwrap();
        assert_eq!(log.snapshot(5, 0).unwrap().get_metadata().index, 7);
        assert_eq!(log.pending_snapshot_request(), None);
    }

    #[test]
    fn hard_state_and_conf_state_survive_reopening() {
        // Raft's term and vote must outlive a restart: forgetting a vote is
        // how one node votes twice in a term and elects two leaders.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.db");
        {
            let log = RaftLog::open(&path).unwrap();
            let mut hs = HardState::default();
            hs.term = 7;
            hs.vote = 3;
            hs.commit = 11;
            log.set_hard_state(&hs).unwrap();

            let mut cs = ConfState::default();
            cs.voters = vec![1, 2, 3];
            log.set_conf_state(&cs).unwrap();
        }
        let log = RaftLog::open(&path).unwrap();
        let state = log.initial_state().unwrap();
        assert_eq!(state.hard_state.term, 7);
        assert_eq!(state.hard_state.vote, 3);
        assert_eq!(state.hard_state.commit, 11);
        assert_eq!(state.conf_state.voters, vec![1, 2, 3]);
    }

    #[test]
    fn max_size_still_yields_at_least_one_entry() {
        // A cap that returns nothing would stall replication forever on the
        // first entry larger than it.
        let (log, _d) = log();
        log.append(&[entry(1, 1, &vec![0u8; 4096]), entry(2, 1, &vec![0u8; 4096])]).unwrap();
        let got = log.entries(1, 3, Some(1u64), GetEntriesContext::empty(false)).unwrap();
        assert_eq!(got.len(), 1, "one oversized entry must still be delivered");
    }

    #[test]
    fn entries_are_capped_by_max_size_when_they_can_be() {
        let (log, _d) = log();
        log.append(&[entry(1, 1, &vec![7u8; 100]), entry(2, 1, &vec![7u8; 100]), entry(3, 1, &vec![7u8; 100])])
            .unwrap();
        let got = log.entries(1, 4, Some(150u64), GetEntriesContext::empty(false)).unwrap();
        assert!(got.len() < 3, "the cap should have stopped it short, got {}", got.len());
        assert!(!got.is_empty());
    }

    #[test]
    fn the_log_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.db");
        {
            let log = RaftLog::open(&path).unwrap();
            log.append(&[entry(1, 1, b"persisted")]).unwrap();
        }
        let log = RaftLog::open(&path).unwrap();
        assert_eq!(log.last_index().unwrap(), 1);
        assert_eq!(
            log.entries(1, 2, None, GetEntriesContext::empty(false)).unwrap()[0].data.as_ref(),
            b"persisted"
        );
    }
}
