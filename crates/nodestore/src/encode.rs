//! Log entry encoding: [`crate::command`] types ⇄ `proto/command.proto`.
//!
//! Every raft log entry passes through here in both directions — written by
//! the proposer, read back by every replica and by this node's own restart.
//! It is therefore the one place where a mistake is not a bug but a data
//! format problem: an entry that encodes wrongly is wrong forever, on every
//! replica, and re-reading it will not fix it.
//!
//! Hence the round-trip tests at the bottom, which assert the property that
//! actually matters — decode(encode(x)) == x — over every variant, rather
//! than checking field-by-field that the encoder does what it obviously does.

use crate::command::{
    Command, Compare, CompareResult, CompareTarget, DeleteOp, KeyRange, Member, PutOp, RangeQuery,
    RequestOp, Sort, SortTarget, TxnOp,
};
use crate::error::{Error, Result};
use crate::pb::log as pb;
use prost::Message as _;

/// Encode a command and its proposal id into raft log entry bytes.
pub fn encode_entry(proposal_id: u64, cmd: &Command) -> Vec<u8> {
    pb::LogEntry { proposal_id, command: Some(command_to_pb(cmd)) }.encode_to_vec()
}

/// Decode a raft log entry.
///
/// A failure here means a committed entry cannot be applied, which is not
/// recoverable by retrying: the entry is already durable on a quorum. It is
/// surfaced as an error so the caller can refuse to continue rather than skip
/// the entry — skipping would silently diverge this replica from the others.
pub fn decode_entry(bytes: &[u8]) -> Result<(u64, Command)> {
    let entry = pb::LogEntry::decode(bytes)
        .map_err(|e| Error::InvalidRequest(format!("undecodable raft log entry: {e}")))?;
    let command = entry
        .command
        .ok_or_else(|| Error::InvalidRequest("raft log entry carries no command".to_string()))?;
    Ok((entry.proposal_id, command_from_pb(command)?))
}

// ── Command ──────────────────────────────────────────────────────────────

pub fn command_to_pb(cmd: &Command) -> pb::Command {
    use pb::command::Kind;
    let kind = match cmd {
        Command::Put(op) => Kind::Put(put_to_pb(op)),
        Command::Delete(op) => Kind::Delete(delete_to_pb(op)),
        Command::Txn(op) => Kind::Txn(pb::TxnOp {
            compare: op.compare.iter().map(compare_to_pb).collect(),
            success: op.success.iter().map(request_op_to_pb).collect(),
            failure: op.failure.iter().map(request_op_to_pb).collect(),
        }),
        Command::Compact { revision } => Kind::Compact(pb::Compact { revision: *revision }),
        Command::LeaseGrant { id, ttl_secs, now_unix_secs } => Kind::LeaseGrant(pb::LeaseGrant {
            id: *id,
            ttl_secs: *ttl_secs,
            now_unix_secs: *now_unix_secs,
        }),
        Command::LeaseRevoke { id } => Kind::LeaseRevoke(pb::LeaseRevoke { id: *id }),
        Command::LeaseKeepAlive { id, now_unix_secs } => {
            Kind::LeaseKeepAlive(pb::LeaseKeepAlive { id: *id, now_unix_secs: *now_unix_secs })
        }
        Command::ExpireLeases { now_unix_secs } => {
            Kind::ExpireLeases(pb::ExpireLeases { now_unix_secs: *now_unix_secs })
        }
        Command::SetMember(m) => Kind::SetMember(member_to_pb(m)),
        Command::RemoveMember { id } => Kind::RemoveMember(pb::RemoveMember { id: *id }),
    };
    pb::Command { kind: Some(kind) }
}

pub fn command_from_pb(cmd: pb::Command) -> Result<Command> {
    use pb::command::Kind;
    let kind = cmd.kind.ok_or_else(|| Error::InvalidRequest("command has no kind".to_string()))?;
    Ok(match kind {
        Kind::Put(op) => Command::Put(put_from_pb(op)),
        Kind::Delete(op) => Command::Delete(delete_from_pb(op)?),
        Kind::Txn(op) => Command::Txn(TxnOp {
            compare: op.compare.into_iter().map(compare_from_pb).collect::<Result<_>>()?,
            success: op.success.into_iter().map(request_op_from_pb).collect::<Result<_>>()?,
            failure: op.failure.into_iter().map(request_op_from_pb).collect::<Result<_>>()?,
        }),
        Kind::Compact(c) => Command::Compact { revision: c.revision },
        Kind::LeaseGrant(l) => Command::LeaseGrant {
            id: l.id,
            ttl_secs: l.ttl_secs,
            now_unix_secs: l.now_unix_secs,
        },
        Kind::LeaseRevoke(l) => Command::LeaseRevoke { id: l.id },
        Kind::LeaseKeepAlive(l) => {
            Command::LeaseKeepAlive { id: l.id, now_unix_secs: l.now_unix_secs }
        }
        Kind::ExpireLeases(e) => Command::ExpireLeases { now_unix_secs: e.now_unix_secs },
        Kind::SetMember(m) => Command::SetMember(member_from_pb(m)),
        Kind::RemoveMember(m) => Command::RemoveMember { id: m.id },
    })
}

pub fn member_to_pb(m: &Member) -> pb::SetMember {
    pb::SetMember {
        id: m.id,
        peer_url: m.peer_url.clone(),
        client_url: m.client_url.clone(),
        name: m.name.clone(),
        is_learner: m.is_learner,
    }
}

pub fn member_from_pb(m: pb::SetMember) -> Member {
    Member {
        id: m.id,
        peer_url: m.peer_url,
        client_url: m.client_url,
        name: m.name,
        is_learner: m.is_learner,
    }
}

// ── Pieces ───────────────────────────────────────────────────────────────

fn key_range_to_pb(range: &KeyRange) -> pb::KeyRange {
    use pb::key_range::Kind;
    let kind = match range {
        KeyRange::Single(k) => Kind::Single(k.clone()),
        KeyRange::Between { from, to } => {
            Kind::Between(pb::Interval { from: from.clone(), to: to.clone() })
        }
        KeyRange::From(f) => Kind::From(f.clone()),
        KeyRange::All => Kind::All(pb::Empty {}),
    };
    pb::KeyRange { kind: Some(kind) }
}

fn key_range_from_pb(range: Option<pb::KeyRange>) -> Result<KeyRange> {
    use pb::key_range::Kind;
    // A missing range is not "everything" — defaulting here would turn a
    // corrupt delete entry into a delete of the entire store.
    let kind = range
        .and_then(|r| r.kind)
        .ok_or_else(|| Error::InvalidRequest("log entry has no key range".to_string()))?;
    Ok(match kind {
        Kind::Single(k) => KeyRange::Single(k),
        Kind::Between(i) => KeyRange::Between { from: i.from, to: i.to },
        Kind::From(f) => KeyRange::From(f),
        Kind::All(_) => KeyRange::All,
    })
}

fn put_to_pb(op: &PutOp) -> pb::PutOp {
    pb::PutOp {
        key: op.key.clone(),
        value: op.value.clone(),
        lease: op.lease,
        prev_kv: op.prev_kv,
        ignore_value: op.ignore_value,
        ignore_lease: op.ignore_lease,
    }
}

fn put_from_pb(op: pb::PutOp) -> PutOp {
    PutOp {
        key: op.key,
        value: op.value,
        lease: op.lease,
        prev_kv: op.prev_kv,
        ignore_value: op.ignore_value,
        ignore_lease: op.ignore_lease,
    }
}

fn delete_to_pb(op: &DeleteOp) -> pb::DeleteOp {
    pb::DeleteOp { range: Some(key_range_to_pb(&op.range)), prev_kv: op.prev_kv }
}

fn delete_from_pb(op: pb::DeleteOp) -> Result<DeleteOp> {
    Ok(DeleteOp { range: key_range_from_pb(op.range)?, prev_kv: op.prev_kv })
}

fn range_query_to_pb(q: &RangeQuery) -> pb::RangeQuery {
    pb::RangeQuery {
        range: Some(key_range_to_pb(&q.range)),
        revision: q.revision,
        limit: q.limit,
        keys_only: q.keys_only,
        count_only: q.count_only,
        sort: q.sort.map(|s| pb::Sort {
            target: match s.target {
                SortTarget::Key => pb::SortTarget::Key as i32,
                SortTarget::Version => pb::SortTarget::Version as i32,
                SortTarget::CreateRevision => pb::SortTarget::CreateRevision as i32,
                SortTarget::ModRevision => pb::SortTarget::ModRevision as i32,
                SortTarget::Value => pb::SortTarget::Value as i32,
            },
            ascending: s.ascending,
        }),
    }
}

fn range_query_from_pb(q: pb::RangeQuery) -> Result<RangeQuery> {
    let sort = match q.sort {
        None => None,
        Some(s) => Some(Sort {
            target: match pb::SortTarget::try_from(s.target) {
                Ok(pb::SortTarget::Key) => SortTarget::Key,
                Ok(pb::SortTarget::Version) => SortTarget::Version,
                Ok(pb::SortTarget::CreateRevision) => SortTarget::CreateRevision,
                Ok(pb::SortTarget::ModRevision) => SortTarget::ModRevision,
                Ok(pb::SortTarget::Value) => SortTarget::Value,
                Err(_) => {
                    return Err(Error::InvalidRequest(format!("unknown sort target {}", s.target)))
                }
            },
            ascending: s.ascending,
        }),
    };
    Ok(RangeQuery {
        range: key_range_from_pb(q.range)?,
        revision: q.revision,
        limit: q.limit,
        keys_only: q.keys_only,
        count_only: q.count_only,
        sort,
    })
}

fn compare_to_pb(c: &Compare) -> pb::Compare {
    use pb::compare::Target;
    pb::Compare {
        key: c.key.clone(),
        result: match c.result {
            CompareResult::Equal => pb::CompareResult::Equal as i32,
            CompareResult::Greater => pb::CompareResult::Greater as i32,
            CompareResult::Less => pb::CompareResult::Less as i32,
            CompareResult::NotEqual => pb::CompareResult::NotEqual as i32,
        },
        target: Some(match &c.target {
            CompareTarget::Version(v) => Target::Version(*v),
            CompareTarget::CreateRevision(v) => Target::CreateRevision(*v),
            CompareTarget::ModRevision(v) => Target::ModRevision(*v),
            CompareTarget::Value(v) => Target::Value(v.clone()),
            CompareTarget::Lease(v) => Target::Lease(*v),
        }),
    }
}

fn compare_from_pb(c: pb::Compare) -> Result<Compare> {
    use pb::compare::Target;
    let result = match pb::CompareResult::try_from(c.result) {
        Ok(pb::CompareResult::Equal) => CompareResult::Equal,
        Ok(pb::CompareResult::Greater) => CompareResult::Greater,
        Ok(pb::CompareResult::Less) => CompareResult::Less,
        Ok(pb::CompareResult::NotEqual) => CompareResult::NotEqual,
        Err(_) => {
            return Err(Error::InvalidRequest(format!("unknown compare result {}", c.result)))
        }
    };
    // Same reasoning as the wire-side decoder: a comparison with no target
    // would otherwise evaluate against zero and let a compare-and-swap win
    // that should have lost.
    let target = match c.target {
        Some(Target::Version(v)) => CompareTarget::Version(v),
        Some(Target::CreateRevision(v)) => CompareTarget::CreateRevision(v),
        Some(Target::ModRevision(v)) => CompareTarget::ModRevision(v),
        Some(Target::Value(v)) => CompareTarget::Value(v),
        Some(Target::Lease(v)) => CompareTarget::Lease(v),
        None => {
            return Err(Error::InvalidRequest("comparison in log entry has no target".to_string()))
        }
    };
    Ok(Compare { key: c.key, result, target })
}

fn request_op_to_pb(op: &RequestOp) -> pb::RequestOp {
    use pb::request_op::Op;
    pb::RequestOp {
        op: Some(match op {
            RequestOp::Range(q) => Op::Range(range_query_to_pb(q)),
            RequestOp::Put(p) => Op::Put(put_to_pb(p)),
            RequestOp::Delete(d) => Op::Delete(delete_to_pb(d)),
        }),
    }
}

fn request_op_from_pb(op: pb::RequestOp) -> Result<RequestOp> {
    use pb::request_op::Op;
    Ok(match op.op {
        Some(Op::Range(q)) => RequestOp::Range(range_query_from_pb(q)?),
        Some(Op::Put(p)) => RequestOp::Put(put_from_pb(p)),
        Some(Op::Delete(d)) => RequestOp::Delete(delete_from_pb(d)?),
        None => {
            return Err(Error::InvalidRequest("transaction op in log entry is empty".to_string()))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(cmd: Command) {
        let bytes = encode_entry(42, &cmd);
        let (id, decoded) = decode_entry(&bytes).expect("decode");
        assert_eq!(id, 42);
        assert_eq!(decoded, cmd, "round trip changed the command");
    }

    #[test]
    fn put_round_trips() {
        round_trip(Command::Put(PutOp {
            key: b"/registry/pods/default/x".to_vec(),
            // Deliberately not UTF-8: values are arbitrary bytes, and an
            // encoding that quietly required text would corrupt every
            // protobuf-serialized object apiserver stores.
            value: vec![0x00, 0xff, 0x1b, 0x80, b'{'],
            lease: 7,
            prev_kv: true,
            ignore_value: false,
            ignore_lease: true,
        }));
    }

    #[test]
    fn every_key_range_shape_round_trips() {
        for range in [
            KeyRange::Single(b"/a".to_vec()),
            KeyRange::Between { from: b"/a".to_vec(), to: b"/b".to_vec() },
            KeyRange::From(b"/a".to_vec()),
            KeyRange::All,
        ] {
            round_trip(Command::Delete(DeleteOp { range, prev_kv: true }));
        }
    }

    #[test]
    fn every_compare_target_round_trips() {
        for target in [
            CompareTarget::Version(1),
            CompareTarget::CreateRevision(2),
            CompareTarget::ModRevision(3),
            CompareTarget::Value(vec![0xde, 0xad]),
            CompareTarget::Lease(4),
        ] {
            round_trip(Command::Txn(TxnOp {
                compare: vec![Compare {
                    key: b"/k".to_vec(),
                    result: CompareResult::NotEqual,
                    target,
                }],
                success: vec![],
                failure: vec![],
            }));
        }
    }

    #[test]
    fn the_full_apiserver_transaction_round_trips() {
        // The exact shape every kube-apiserver write takes.
        round_trip(Command::Txn(TxnOp {
            compare: vec![Compare {
                key: b"/registry/x".to_vec(),
                result: CompareResult::Equal,
                target: CompareTarget::ModRevision(99),
            }],
            success: vec![RequestOp::Put(PutOp {
                key: b"/registry/x".to_vec(),
                value: b"new".to_vec(),
                lease: 0,
                prev_kv: false,
                ignore_value: false,
                ignore_lease: false,
            })],
            failure: vec![RequestOp::Range(RangeQuery::current(KeyRange::Single(
                b"/registry/x".to_vec(),
            )))],
        }));
    }

    #[test]
    fn sorted_range_queries_round_trip() {
        let mut q = RangeQuery::current(KeyRange::All);
        q.sort = Some(Sort { target: SortTarget::ModRevision, ascending: false });
        q.limit = 500;
        q.keys_only = true;
        round_trip(Command::Txn(TxnOp {
            compare: vec![],
            success: vec![RequestOp::Range(q)],
            failure: vec![],
        }));
    }

    #[test]
    fn lease_and_member_commands_round_trip() {
        round_trip(Command::LeaseGrant { id: 5, ttl_secs: 60, now_unix_secs: 1_700_000_000 });
        round_trip(Command::LeaseRevoke { id: 5 });
        round_trip(Command::LeaseKeepAlive { id: 5, now_unix_secs: 1_700_000_001 });
        round_trip(Command::ExpireLeases { now_unix_secs: 1_700_000_002 });
        round_trip(Command::Compact { revision: 12 });
        round_trip(Command::SetMember(Member {
            id: 2,
            peer_url: "http://10.0.0.2:2380".to_string(),
            client_url: "http://10.0.0.2:2379".to_string(),
            name: "node-2".to_string(),
            is_learner: true,
        }));
        round_trip(Command::RemoveMember { id: 2 });
    }

    #[test]
    fn a_delete_with_no_range_is_refused_rather_than_deleting_everything() {
        // The single worst decode default available. An absent range must be
        // an error, never KeyRange::All.
        let entry = pb::LogEntry {
            proposal_id: 1,
            command: Some(pb::Command {
                kind: Some(pb::command::Kind::Delete(pb::DeleteOp { range: None, prev_kv: false })),
            }),
        };
        assert!(decode_entry(&entry.encode_to_vec()).is_err());
    }

    #[test]
    fn a_comparison_with_no_target_is_refused() {
        let entry = pb::LogEntry {
            proposal_id: 1,
            command: Some(pb::Command {
                kind: Some(pb::command::Kind::Txn(pb::TxnOp {
                    compare: vec![pb::Compare {
                        key: b"/k".to_vec(),
                        result: pb::CompareResult::Equal as i32,
                        target: None,
                    }],
                    success: vec![],
                    failure: vec![],
                })),
            }),
        };
        assert!(decode_entry(&entry.encode_to_vec()).is_err());
    }

    #[test]
    fn garbage_bytes_are_an_error_not_a_panic() {
        // A committed entry that cannot be decoded is unrecoverable, so the
        // applier has to be able to refuse it deliberately rather than
        // unwinding through the raft loop.
        assert!(decode_entry(&[0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(decode_entry(&[]).is_err(), "an empty entry carries no command");
    }
}
