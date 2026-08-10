//! The etcd v3 gRPC surface.
//!
//! Five services are served: `KV`, `Watch`, `Lease`, `Maintenance` and
//! `Cluster`. `Auth` is deliberately not registered — a client asking for it
//! gets `Unimplemented`, which is a truthful answer, rather than a stub that
//! accepts credentials and enforces nothing.
//!
//! Everything here is translation and plumbing. Behaviour lives in
//! [`crate::store`] (semantics) and [`crate::consensus`] (ordering), and a
//! decision made in this layer is almost always a decision in the wrong place.

pub mod convert;
pub mod watch;

use crate::command::{Command, KeyRange, RangeQuery};
use crate::consensus::Node;
use crate::error::Result;
use crate::pb::etcdserverpb as pb;
use crate::store::{CommandResponse, OpResponse};
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The etcd API version reported by `Maintenance.Status`.
///
/// This is the version of the *protocol* being spoken, not a claim to be etcd:
/// clients gate behaviour on it (kube-apiserver refuses a datastore below 3.4),
/// and there is no field in which to answer "nodestore 0.1.0" without them
/// treating it as an unparseable etcd version and refusing to start. kine
/// reports an etcd version for the same reason.
const ETCD_API_VERSION: &str = "3.5.16";

#[derive(Clone)]
pub struct EtcdApi {
    node: Arc<Node>,
}

impl EtcdApi {
    pub fn new(node: Arc<Node>) -> EtcdApi {
        EtcdApi { node }
    }

    pub fn node(&self) -> &Arc<Node> {
        &self.node
    }

    fn header(&self, revision: i64) -> Option<pb::ResponseHeader> {
        Some(pb::ResponseHeader {
            cluster_id: self.node.cluster_id(),
            member_id: self.node.member_id(),
            revision,
            raft_term: self.node.term(),
        })
    }

    fn current_revision(&self) -> Result<i64> {
        self.node.read(|s| s.revision())
    }
}

/// Seconds since the epoch — the leader's clock reading, taken before a
/// command is proposed and never during apply. See [`crate::command`].
pub fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tonic::async_trait]
impl pb::kv_server::Kv for EtcdApi {
    async fn range(
        &self,
        request: Request<pb::RangeRequest>,
    ) -> std::result::Result<Response<pb::RangeResponse>, Status> {
        let req = request.into_inner();
        let query = convert::range_query(&req)?;
        let (result, revision) = self.node.read(|s| {
            let result = s.range(&query)?;
            // The header revision is the store's current revision, even for a
            // historical read: it tells the client how far the store has got,
            // which is not the same question as what it just read.
            Ok((result, s.revision()?))
        })?;

        Ok(Response::new(pb::RangeResponse {
            header: self.header(revision),
            kvs: result.kvs.iter().map(convert::key_value_to_pb).collect(),
            more: result.more,
            count: result.count,
        }))
    }

    async fn put(
        &self,
        request: Request<pb::PutRequest>,
    ) -> std::result::Result<Response<pb::PutResponse>, Status> {
        let req = request.into_inner();
        let applied = self.node.propose(Command::Put(convert::put_op(&req))).await?;
        let prev_kv = match &applied.response {
            CommandResponse::Put { prev_kv } => prev_kv.as_ref().map(convert::key_value_to_pb),
            _ => None,
        };
        Ok(Response::new(pb::PutResponse { header: self.header(applied.revision), prev_kv }))
    }

    async fn delete_range(
        &self,
        request: Request<pb::DeleteRangeRequest>,
    ) -> std::result::Result<Response<pb::DeleteRangeResponse>, Status> {
        let req = request.into_inner();
        let applied = self.node.propose(Command::Delete(convert::delete_op(&req))).await?;
        let (deleted, prev_kvs) = match &applied.response {
            CommandResponse::Delete { deleted, prev_kvs } => {
                (*deleted, prev_kvs.iter().map(convert::key_value_to_pb).collect())
            }
            _ => (0, Vec::new()),
        };
        Ok(Response::new(pb::DeleteRangeResponse {
            header: self.header(applied.revision),
            deleted,
            prev_kvs,
        }))
    }

    async fn txn(
        &self,
        request: Request<pb::TxnRequest>,
    ) -> std::result::Result<Response<pb::TxnResponse>, Status> {
        let req = request.into_inner();
        let applied = self.node.propose(Command::Txn(convert::txn_op(&req)?)).await?;
        let (succeeded, responses) = match &applied.response {
            CommandResponse::Txn { succeeded, responses } => (*succeeded, responses.clone()),
            _ => (false, Vec::new()),
        };

        let header = self.header(applied.revision);
        let responses = responses
            .iter()
            .map(|op| pb::ResponseOp {
                response: Some(match op {
                    OpResponse::Range(r) => {
                        pb::response_op::Response::ResponseRange(pb::RangeResponse {
                            header: header.clone(),
                            kvs: r.kvs.iter().map(convert::key_value_to_pb).collect(),
                            more: r.more,
                            count: r.count,
                        })
                    }
                    OpResponse::Put { prev_kv } => {
                        pb::response_op::Response::ResponsePut(pb::PutResponse {
                            header: header.clone(),
                            prev_kv: prev_kv.as_ref().map(convert::key_value_to_pb),
                        })
                    }
                    OpResponse::Delete { deleted, prev_kvs } => {
                        pb::response_op::Response::ResponseDeleteRange(pb::DeleteRangeResponse {
                            header: header.clone(),
                            deleted: *deleted,
                            prev_kvs: prev_kvs.iter().map(convert::key_value_to_pb).collect(),
                        })
                    }
                }),
            })
            .collect();

        Ok(Response::new(pb::TxnResponse { header, succeeded, responses }))
    }

    async fn compact(
        &self,
        request: Request<pb::CompactionRequest>,
    ) -> std::result::Result<Response<pb::CompactionResponse>, Status> {
        let req = request.into_inner();
        // `physical` asks etcd to wait until the space is actually reclaimed.
        // Here the delete is part of the same transaction that moves the
        // compaction point, so the request is already satisfied by the time it
        // returns and there is nothing extra to wait for.
        let applied = self.node.propose(Command::Compact { revision: req.revision }).await?;
        Ok(Response::new(pb::CompactionResponse { header: self.header(applied.revision) }))
    }
}

#[tonic::async_trait]
impl pb::lease_server::Lease for EtcdApi {
    async fn lease_grant(
        &self,
        request: Request<pb::LeaseGrantRequest>,
    ) -> std::result::Result<Response<pb::LeaseGrantResponse>, Status> {
        let req = request.into_inner();
        // id 0 means "pick one for me". Chosen here, on the leader, so the
        // command that reaches every replica names the same lease.
        let id = if req.id == 0 { self.node.allocate_lease_id() } else { req.id };
        let applied = self
            .node
            .propose(Command::LeaseGrant { id, ttl_secs: req.ttl, now_unix_secs: now_unix_secs() })
            .await?;
        let ttl = match applied.response {
            CommandResponse::Lease { ttl_secs } => ttl_secs,
            _ => req.ttl,
        };
        Ok(Response::new(pb::LeaseGrantResponse {
            header: self.header(applied.revision),
            id,
            ttl,
            error: String::new(),
        }))
    }

    async fn lease_revoke(
        &self,
        request: Request<pb::LeaseRevokeRequest>,
    ) -> std::result::Result<Response<pb::LeaseRevokeResponse>, Status> {
        let req = request.into_inner();
        let applied = self.node.propose(Command::LeaseRevoke { id: req.id }).await?;
        Ok(Response::new(pb::LeaseRevokeResponse { header: self.header(applied.revision) }))
    }

    type LeaseKeepAliveStream = std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = std::result::Result<pb::LeaseKeepAliveResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn lease_keep_alive(
        &self,
        request: Request<tonic::Streaming<pb::LeaseKeepAliveRequest>>,
    ) -> std::result::Result<Response<Self::LeaseKeepAliveStream>, Status> {
        let mut inbound = request.into_inner();
        let api = self.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            while let Ok(Some(req)) = inbound.message().await {
                let applied = api
                    .node
                    .propose(Command::LeaseKeepAlive {
                        id: req.id,
                        now_unix_secs: now_unix_secs(),
                    })
                    .await;
                let response = match applied {
                    Ok(applied) => {
                        let ttl = match applied.response {
                            CommandResponse::Lease { ttl_secs } => ttl_secs,
                            _ => 0,
                        };
                        Ok(pb::LeaseKeepAliveResponse {
                            header: api.header(applied.revision),
                            id: req.id,
                            // TTL 0 is etcd's way of saying "that lease is
                            // gone" — not an error, and clients act on it.
                            ttl,
                        })
                    }
                    Err(e) => Err(Status::from(e)),
                };
                if tx.send(response).await.is_err() {
                    break; // client hung up
                }
            }
        });

        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn lease_time_to_live(
        &self,
        request: Request<pb::LeaseTimeToLiveRequest>,
    ) -> std::result::Result<Response<pb::LeaseTimeToLiveResponse>, Status> {
        let req = request.into_inner();
        let now = now_unix_secs();
        let (lease, keys, revision) = self.node.read(|s| {
            let lease = s.lease_ttl(req.id)?;
            let keys = if req.keys && lease.is_some() { s.lease_keys(req.id)? } else { Vec::new() };
            Ok((lease, keys, s.revision()?))
        })?;

        // etcd reports TTL -1 for a lease that does not exist, which is
        // distinct from 0 ("expires now").
        let (ttl, granted_ttl) = match lease {
            None => (-1, 0),
            Some((granted, expires_at)) => ((expires_at - now).max(0), granted),
        };
        Ok(Response::new(pb::LeaseTimeToLiveResponse {
            header: self.header(revision),
            id: req.id,
            ttl,
            granted_ttl,
            keys,
        }))
    }

    async fn lease_leases(
        &self,
        _request: Request<pb::LeaseLeasesRequest>,
    ) -> std::result::Result<Response<pb::LeaseLeasesResponse>, Status> {
        let (ids, revision) = self.node.read(|s| Ok((s.leases()?, s.revision()?)))?;
        Ok(Response::new(pb::LeaseLeasesResponse {
            header: self.header(revision),
            leases: ids.into_iter().map(|id| pb::LeaseStatus { id }).collect(),
        }))
    }
}

#[tonic::async_trait]
impl pb::maintenance_server::Maintenance for EtcdApi {
    async fn status(
        &self,
        _request: Request<pb::StatusRequest>,
    ) -> std::result::Result<Response<pb::StatusResponse>, Status> {
        let revision = self.current_revision()?;
        Ok(Response::new(pb::StatusResponse {
            header: self.header(revision),
            version: ETCD_API_VERSION.to_string(),
            db_size: 0,
            leader: self.node.member_id(),
            raft_index: revision as u64,
            raft_term: self.node.term(),
            raft_applied_index: revision as u64,
            errors: Vec::new(),
            db_size_in_use: 0,
            is_learner: false,
        }))
    }

    async fn alarm(
        &self,
        _request: Request<pb::AlarmRequest>,
    ) -> std::result::Result<Response<pb::AlarmResponse>, Status> {
        // No alarms is a real answer, not a stub: the alarms etcd raises
        // (NOSPACE, CORRUPT) are conditions this store does not have a
        // concept of yet, and reporting none is accurate for it.
        let revision = self.current_revision()?;
        Ok(Response::new(pb::AlarmResponse { header: self.header(revision), alarms: Vec::new() }))
    }

    async fn defragment(
        &self,
        _request: Request<pb::DefragmentRequest>,
    ) -> std::result::Result<Response<pb::DefragmentResponse>, Status> {
        // sqlite's equivalent is VACUUM, which rewrites the whole database and
        // needs as much free space again. Refusing is better than doing that
        // unannounced on an edge device.
        Err(Status::unimplemented(
            "nodestore: defragment is not implemented; space is reclaimed by compaction",
        ))
    }

    async fn hash(
        &self,
        _request: Request<pb::HashRequest>,
    ) -> std::result::Result<Response<pb::HashResponse>, Status> {
        Err(Status::unimplemented("nodestore: hash is not implemented"))
    }

    async fn hash_kv(
        &self,
        _request: Request<pb::HashKvRequest>,
    ) -> std::result::Result<Response<pb::HashKvResponse>, Status> {
        Err(Status::unimplemented("nodestore: hash_kv is not implemented"))
    }

    type SnapshotStream = std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = std::result::Result<pb::SnapshotResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn snapshot(
        &self,
        _request: Request<pb::SnapshotRequest>,
    ) -> std::result::Result<Response<Self::SnapshotStream>, Status> {
        Err(Status::unimplemented(
            "nodestore: snapshot is not implemented; back up the sqlite database directly",
        ))
    }

    async fn move_leader(
        &self,
        _request: Request<pb::MoveLeaderRequest>,
    ) -> std::result::Result<Response<pb::MoveLeaderResponse>, Status> {
        Err(Status::unimplemented("nodestore: single member; there is nowhere to move leadership"))
    }

    async fn downgrade(
        &self,
        _request: Request<pb::DowngradeRequest>,
    ) -> std::result::Result<Response<pb::DowngradeResponse>, Status> {
        Err(Status::unimplemented("nodestore: downgrade is not implemented"))
    }
}

#[tonic::async_trait]
impl pb::cluster_server::Cluster for EtcdApi {
    async fn member_list(
        &self,
        _request: Request<pb::MemberListRequest>,
    ) -> std::result::Result<Response<pb::MemberListResponse>, Status> {
        let revision = self.current_revision()?;
        Ok(Response::new(pb::MemberListResponse {
            header: self.header(revision),
            members: vec![pb::Member {
                id: self.node.member_id(),
                name: "nodestore".to_string(),
                peer_ur_ls: Vec::new(),
                client_ur_ls: Vec::new(),
                is_learner: false,
            }],
        }))
    }

    async fn member_add(
        &self,
        _request: Request<pb::MemberAddRequest>,
    ) -> std::result::Result<Response<pb::MemberAddResponse>, Status> {
        // The honest error for the feature this whole design is built toward
        // but has not reached. Accepting a member and never replicating to it
        // would be far worse than refusing.
        Err(Status::unimplemented(
            "nodestore: multi-node raft replication is not implemented yet; this member cannot add peers",
        ))
    }

    async fn member_remove(
        &self,
        _request: Request<pb::MemberRemoveRequest>,
    ) -> std::result::Result<Response<pb::MemberRemoveResponse>, Status> {
        Err(Status::unimplemented("nodestore: single member; there is nothing to remove"))
    }

    async fn member_update(
        &self,
        _request: Request<pb::MemberUpdateRequest>,
    ) -> std::result::Result<Response<pb::MemberUpdateResponse>, Status> {
        Err(Status::unimplemented("nodestore: single member; there is nothing to update"))
    }

    async fn member_promote(
        &self,
        _request: Request<pb::MemberPromoteRequest>,
    ) -> std::result::Result<Response<pb::MemberPromoteResponse>, Status> {
        Err(Status::unimplemented("nodestore: single member; there are no learners to promote"))
    }
}

/// Auto-compaction: keep the last `retain` revisions and discard the rest.
///
/// apiserver compacts on its own schedule too, and both are welcome — whoever
/// gets there first moves the point and the other's request is a no-op or an
/// already-compacted error, which is handled. This loop exists because a store
/// nobody compacts grows without bound, and the device this runs on is the one
/// that can least afford that.
pub async fn compaction_loop(api: EtcdApi, interval_secs: u64, retain: i64) {
    if interval_secs == 0 {
        tracing::info!("auto-compaction disabled (NODESTORE_COMPACT_INTERVAL_SECS=0)");
        return;
    }
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;
        let target = match api.node.read(|s| Ok((s.revision()?, s.compact_revision()?))) {
            Ok((current, compacted)) => {
                let target = current - retain;
                if target <= compacted {
                    continue; // nothing new to discard
                }
                target
            }
            Err(e) => {
                tracing::warn!(error = %e, "auto-compaction: could not read the current revision");
                continue;
            }
        };
        match api.node.propose(Command::Compact { revision: target }).await {
            Ok(_) => tracing::debug!(revision = target, "auto-compacted"),
            Err(e) => tracing::warn!(error = %e, revision = target, "auto-compaction failed"),
        }
    }
}

/// Expire leases whose TTL has run out.
///
/// Reads first and only proposes when something has actually expired: an idle
/// store must stay idle, which is the entire point of replacing a datastore
/// that polled.
pub async fn lease_expiry_loop(api: EtcdApi, interval_secs: u64) {
    let mut ticker =
        tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    loop {
        ticker.tick().await;
        let now = now_unix_secs();
        match api.node.read(|s| s.expired_leases(now)) {
            Ok(expired) if expired.is_empty() => continue,
            Ok(expired) => {
                tracing::debug!(count = expired.len(), "expiring leases");
                if let Err(e) = api.node.propose(Command::ExpireLeases { now_unix_secs: now }).await
                {
                    tracing::warn!(error = %e, "lease expiry failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "lease expiry: could not read leases"),
        }
    }
}

/// A convenience read used by the e2e tests and by `nodestore --probe`.
pub fn count_keys(node: &Node) -> Result<i64> {
    node.read(|s| {
        let mut q = RangeQuery::current(KeyRange::All);
        q.count_only = true;
        Ok(s.range(&q)?.count)
    })
}
