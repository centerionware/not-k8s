//! Wire types in, command types out.
//!
//! Every translation between etcd's protobuf and this crate's own types lives
//! here, in one place, so the rest of the server is about behaviour rather
//! than field shuffling — and so the boundary described in `command.rs` (the
//! wire contract is etcd's, the log contract is ours) has an actual file.

use crate::command::{
    Compare, CompareResult, CompareTarget, DeleteOp, KeyRange, PutOp, RangeQuery, RequestOp, Sort,
    SortTarget, TxnOp,
};
use crate::error::{Error, Result};
use crate::pb::etcdserverpb as pb;
use crate::pb::mvccpb;
use crate::store::{Event, EventKind, KeyValue};

pub fn key_value_to_pb(kv: &KeyValue) -> mvccpb::KeyValue {
    mvccpb::KeyValue {
        key: kv.key.clone(),
        create_revision: kv.create_revision,
        mod_revision: kv.mod_revision,
        version: kv.version,
        value: kv.value.clone(),
        lease: kv.lease,
    }
}

pub fn event_to_pb(event: &Event, want_prev_kv: bool) -> mvccpb::Event {
    mvccpb::Event {
        r#type: match event.kind {
            EventKind::Put => mvccpb::event::EventType::Put as i32,
            EventKind::Delete => mvccpb::event::EventType::Delete as i32,
        },
        kv: Some(key_value_to_pb(&event.kv)),
        prev_kv: if want_prev_kv { event.prev_kv.as_ref().map(key_value_to_pb) } else { None },
    }
}

pub fn range_query(req: &pb::RangeRequest) -> Result<RangeQuery> {
    let sort = match pb::range_request::SortTarget::try_from(req.sort_target) {
        // NONE ordering means etcd's default, which is by key ascending —
        // and not "unordered", which would make paging non-deterministic.
        Ok(_) if req.sort_order == pb::range_request::SortOrder::None as i32 => None,
        Ok(target) => Some(Sort {
            target: match target {
                pb::range_request::SortTarget::Key => SortTarget::Key,
                pb::range_request::SortTarget::Version => SortTarget::Version,
                pb::range_request::SortTarget::Create => SortTarget::CreateRevision,
                pb::range_request::SortTarget::Mod => SortTarget::ModRevision,
                pb::range_request::SortTarget::Value => SortTarget::Value,
            },
            ascending: req.sort_order != pb::range_request::SortOrder::Descend as i32,
        }),
        Err(_) => {
            return Err(Error::InvalidRequest(format!("unknown sort target {}", req.sort_target)))
        }
    };

    Ok(RangeQuery {
        range: KeyRange::decode(req.key.clone(), req.range_end.clone()),
        revision: req.revision,
        limit: req.limit,
        keys_only: req.keys_only,
        count_only: req.count_only,
        sort,
    })
}

pub fn put_op(req: &pb::PutRequest) -> PutOp {
    PutOp {
        key: req.key.clone(),
        value: req.value.clone(),
        lease: req.lease,
        prev_kv: req.prev_kv,
        ignore_value: req.ignore_value,
        ignore_lease: req.ignore_lease,
    }
}

pub fn delete_op(req: &pb::DeleteRangeRequest) -> DeleteOp {
    DeleteOp {
        range: KeyRange::decode(req.key.clone(), req.range_end.clone()),
        prev_kv: req.prev_kv,
    }
}

pub fn txn_op(req: &pb::TxnRequest) -> Result<TxnOp> {
    Ok(TxnOp {
        compare: req.compare.iter().map(compare).collect::<Result<Vec<_>>>()?,
        success: req.success.iter().map(request_op).collect::<Result<Vec<_>>>()?,
        failure: req.failure.iter().map(request_op).collect::<Result<Vec<_>>>()?,
    })
}

fn compare(c: &pb::Compare) -> Result<Compare> {
    let result = match pb::compare::CompareResult::try_from(c.result) {
        Ok(pb::compare::CompareResult::Equal) => CompareResult::Equal,
        Ok(pb::compare::CompareResult::Greater) => CompareResult::Greater,
        Ok(pb::compare::CompareResult::Less) => CompareResult::Less,
        Ok(pb::compare::CompareResult::NotEqual) => CompareResult::NotEqual,
        Err(_) => {
            return Err(Error::InvalidRequest(format!("unknown compare result {}", c.result)))
        }
    };

    // The target lives in a oneof, and its absence is a malformed request
    // rather than a default: a comparison with nothing to compare against
    // would otherwise silently evaluate against zero and let a
    // compare-and-swap succeed when it should not.
    let target = match c.target_union.as_ref() {
        Some(pb::compare::TargetUnion::Version(v)) => CompareTarget::Version(*v),
        Some(pb::compare::TargetUnion::CreateRevision(v)) => CompareTarget::CreateRevision(*v),
        Some(pb::compare::TargetUnion::ModRevision(v)) => CompareTarget::ModRevision(*v),
        Some(pb::compare::TargetUnion::Value(v)) => CompareTarget::Value(v.clone()),
        Some(pb::compare::TargetUnion::Lease(v)) => CompareTarget::Lease(*v),
        None => {
            return Err(Error::InvalidRequest(
                "etcdserver: comparison has no target".to_string(),
            ))
        }
    };

    // range_end on a comparison ("do these hold for every key in the range")
    // is part of etcd's API but is not something apiserver emits. Refusing it
    // is honest; quietly comparing only the first key would corrupt a
    // transaction's meaning.
    if !c.range_end.is_empty() {
        return Err(Error::InvalidRequest(
            "nodestore: comparisons over a key range are not implemented".to_string(),
        ));
    }

    Ok(Compare { key: c.key.clone(), result, target })
}

fn request_op(op: &pb::RequestOp) -> Result<RequestOp> {
    match op.request.as_ref() {
        Some(pb::request_op::Request::RequestRange(r)) => Ok(RequestOp::Range(range_query(r)?)),
        Some(pb::request_op::Request::RequestPut(r)) => Ok(RequestOp::Put(put_op(r))),
        Some(pb::request_op::Request::RequestDeleteRange(r)) => Ok(RequestOp::Delete(delete_op(r))),
        Some(pb::request_op::Request::RequestTxn(_)) => Err(Error::InvalidRequest(
            "nodestore: nested transactions are not implemented".to_string(),
        )),
        None => Err(Error::InvalidRequest("etcdserver: transaction op is empty".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comparison_without_a_target_is_refused() {
        // Defaulting to zero here would turn a malformed request into a
        // silently-successful compare-and-swap.
        let c = pb::Compare {
            key: b"/a".to_vec(),
            result: pb::compare::CompareResult::Equal as i32,
            target: 0,
            range_end: vec![],
            target_union: None,
        };
        assert!(matches!(compare(&c), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn mod_revision_comparisons_survive_the_round_trip() {
        let c = pb::Compare {
            key: b"/a".to_vec(),
            result: pb::compare::CompareResult::Equal as i32,
            target: pb::compare::CompareTarget::Mod as i32,
            range_end: vec![],
            target_union: Some(pb::compare::TargetUnion::ModRevision(7)),
        };
        let out = compare(&c).unwrap();
        assert_eq!(out.target, CompareTarget::ModRevision(7));
        assert_eq!(out.result, CompareResult::Equal);
    }

    #[test]
    fn an_unsorted_range_still_orders_by_key() {
        // etcd's SortOrder::None is "the default order", which is by key —
        // apiserver's paging is only correct under a total order.
        let req = pb::RangeRequest {
            key: vec![0],
            range_end: vec![0],
            sort_order: pb::range_request::SortOrder::None as i32,
            sort_target: pb::range_request::SortTarget::Key as i32,
            ..Default::default()
        };
        assert!(range_query(&req).unwrap().sort.is_none(), "None means the store's default");
    }

    #[test]
    fn descending_sort_is_carried_through() {
        let req = pb::RangeRequest {
            key: vec![0],
            range_end: vec![0],
            sort_order: pb::range_request::SortOrder::Descend as i32,
            sort_target: pb::range_request::SortTarget::Mod as i32,
            ..Default::default()
        };
        let sort = range_query(&req).unwrap().sort.unwrap();
        assert_eq!(sort.target, SortTarget::ModRevision);
        assert!(!sort.ascending);
    }

    #[test]
    fn nested_transactions_are_refused_rather_than_ignored() {
        let op = pb::RequestOp {
            request: Some(pb::request_op::Request::RequestTxn(pb::TxnRequest::default())),
        };
        assert!(matches!(request_op(&op), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn an_empty_transaction_op_is_refused() {
        assert!(matches!(request_op(&pb::RequestOp { request: None }), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn delete_events_carry_the_previous_value_when_asked() {
        let event = Event {
            kind: EventKind::Delete,
            kv: KeyValue { key: b"/a".to_vec(), mod_revision: 5, ..Default::default() },
            prev_kv: Some(KeyValue {
                key: b"/a".to_vec(),
                value: b"gone".to_vec(),
                mod_revision: 4,
                ..Default::default()
            }),
        };
        let with = event_to_pb(&event, true);
        assert_eq!(with.r#type, mvccpb::event::EventType::Delete as i32);
        assert_eq!(with.prev_kv.unwrap().value, b"gone");

        let without = event_to_pb(&event, false);
        assert!(without.prev_kv.is_none(), "prev_kv is opt-in per watch");
    }
}
