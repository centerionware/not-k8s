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

/// Where a write goes when this member is not the leader.
///
/// Followers forward rather than refuse. etcd's clients do tolerate being
/// told "not the leader", but kube-apiserver is configured with a list of
/// endpoints and expects any of them to work — a follower that refused writes
/// would make a three-member cluster serve writes only a third of the time
/// from apiserver's point of view.
///
/// The request is forwarded *verbatim*, using the same generated client the
/// API is defined by, so a forwarded write cannot drift from a local one.
macro_rules! forward_if_follower {
    ($self:ident, $client:ty, $method:ident, $req:expr) => {
        if !$self.node.is_leader() {
            let url = $self.leader_client_url().await?;
            // The leader's client API requires a client certificate like any
            // other caller — a follower forwarding a write is not exempt, and
            // making it exempt would mean the leader accepting unauthenticated
            // writes from anything that could claim to be a peer.
            let channel = $self.dial_leader(&url).await?;
            let mut client = <$client>::new(channel);
            return client.$method($req).await;
        }
    };
}

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
    /// Present only in a real cluster. Its absence is what makes the
    /// membership RPCs answer "single member" rather than pretending.
    raft: Option<crate::replication::driver::RaftHandle>,
    /// Client-domain TLS material, used when *this* member has to act as a
    /// client of another member's client API — i.e. forwarding a write to the
    /// leader.
    client_tls: Option<crate::tls::Material>,
}

impl EtcdApi {
    pub fn new(node: Arc<Node>) -> EtcdApi {
        EtcdApi { node, raft: None, client_tls: None }
    }

    pub fn with_raft(mut self, handle: crate::replication::driver::RaftHandle) -> EtcdApi {
        self.raft = Some(handle);
        self
    }

    pub fn with_client_tls(mut self, material: crate::tls::Material) -> EtcdApi {
        self.client_tls = Some(material);
        self
    }

    /// The raft handle, or the honest error for a single-member store.
    ///
    /// Still `Unimplemented` when there is no cluster — not because the code
    /// is missing any more, but because "add a member to a store that is not
    /// running raft" genuinely has no meaning. Starting it with an initial
    /// cluster is what makes these RPCs available.
    fn raft(&self) -> std::result::Result<&crate::replication::driver::RaftHandle, Status> {
        self.raft.as_ref().ok_or_else(|| {
            Status::unimplemented(
                "nodestore: this member is not running raft (no NODESTORE_INITIAL_CLUSTER), so it \
                 has no membership to change",
            )
        })
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

    /// Dial the leader's client API with this member's own certificate.
    async fn dial_leader(
        &self,
        url: &str,
    ) -> std::result::Result<tonic::transport::Channel, Status> {
        let endpoint = tonic::transport::Endpoint::from_shared(url.to_string())
            .map_err(|e| Status::internal(format!("leader URL {url} is not dialable: {e}")))?;
        let endpoint = match &self.client_tls {
            Some(material) => {
                let host = crate::host_of(url);
                let cfg = crate::tls::client_tls_config(material, host.as_deref())
                    .map_err(|e| Status::internal(format!("building forwarding TLS: {e}")))?;
                endpoint
                    .tls_config(cfg)
                    .map_err(|e| Status::internal(format!("applying forwarding TLS: {e}")))?
            }
            None => endpoint,
        };
        endpoint
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("could not reach the leader at {url}: {e}")))
    }

    /// How long a forward will wait for the cluster to become forwardable
    /// before giving up. Both conditions it waits on resolve in well under
    /// this on a healthy cluster; it exists so a request cannot hang forever,
    /// because apiserver blocks a worker on its storage write and a call that
    /// never returns takes that worker with it.
    const FORWARD_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

    /// The leader's client URL, for forwarding.
    ///
    /// Both of the conditions this can hit are *transient*, and both are
    /// waited on rather than failed:
    ///
    ///   * **No leader right now** — an election is in progress.
    ///   * **A leader whose client URL is not in the address book yet** — the
    ///     book is replicated state, published by a leader when it takes
    ///     office (see [`crate::replication::bootstrap::publish_address_book`]).
    ///     So immediately after an election there is a window where this
    ///     member knows *who* leads but not *how to reach it*.
    ///
    /// That second case was previously reported as a configuration problem
    /// retrying would not fix, and failed instantly. It is the opposite: the
    /// URL is on its way, and the window is milliseconds. Failing inside it
    /// broke the guarantee this whole forwarding path exists to provide —
    /// apiserver is handed an endpoint list and expects any endpoint to accept
    /// a write, so for that window a three-member cluster served writes only
    /// when the request happened to land on the leader.
    ///
    /// Caught by `test_a_follower_forwards_writes_to_the_leader` in the
    /// cluster e2e, which writes to a follower as soon as a leader exists —
    /// i.e. squarely inside the window.
    async fn leader_client_url(&self) -> std::result::Result<String, Status> {
        let deadline = tokio::time::Instant::now() + Self::FORWARD_WAIT;
        // Short enough that the common case (the book lands within a tick or
        // two) costs nothing worth measuring, and this only runs on a
        // follower that is actively forwarding a write.
        let poll = std::time::Duration::from_millis(25);
        let mut last_seen_leader = None;

        loop {
            if let Some(leader) = self.node.leader_id() {
                last_seen_leader = Some(leader);
                let member = self
                    .node
                    .read(|s| s.member(leader))
                    .map_err(|e| Status::internal(format!("reading the address book: {e}")))?;
                if let Some(url) = member.map(|m| m.client_url).filter(|u| !u.is_empty()) {
                    return Ok(url);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(poll).await;
        }

        // Still worth distinguishing in the error, because they point at
        // different things: no leader at all suggests quorum trouble, whereas
        // a leader with no published address after this long suggests its
        // NODESTORE_ADVERTISE_CLIENT_URL is unset or it cannot commit.
        Err(match last_seen_leader {
            Some(leader) => Status::unavailable(format!(
                "member {leader} is the leader but published no client URL within {:?}; \
                 check its NODESTORE_ADVERTISE_CLIENT_URL",
                Self::FORWARD_WAIT
            )),
            None => Status::unavailable(format!(
                "no leader elected within {:?}; retry shortly",
                Self::FORWARD_WAIT
            )),
        })
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
        // `serializable` is the client saying it will accept a possibly-stale
        // local read. Anything else must be answered by the leader, or a
        // follower could serve a value the cluster has already replaced —
        // which for apiserver means a resourceVersion that goes backwards.
        if !req.serializable {
            forward_if_follower!(
                self,
                pb::kv_client::KvClient<tonic::transport::Channel>,
                range,
                req
            );
        }
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
            ..Default::default()
        }))
    }

    async fn put(
        &self,
        request: Request<pb::PutRequest>,
    ) -> std::result::Result<Response<pb::PutResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(self, pb::kv_client::KvClient<tonic::transport::Channel>, put, req);
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
        forward_if_follower!(
            self,
            pb::kv_client::KvClient<tonic::transport::Channel>,
            delete_range,
            req
        );
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
        forward_if_follower!(self, pb::kv_client::KvClient<tonic::transport::Channel>, txn, req);
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
                            ..Default::default()
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
        forward_if_follower!(
            self,
            pb::kv_client::KvClient<tonic::transport::Channel>,
            compact,
            req
        );
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
        forward_if_follower!(
            self,
            pb::lease_client::LeaseClient<tonic::transport::Channel>,
            lease_grant,
            req
        );
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
            ..Default::default()
        }))
    }

    async fn lease_revoke(
        &self,
        request: Request<pb::LeaseRevokeRequest>,
    ) -> std::result::Result<Response<pb::LeaseRevokeResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(
            self,
            pb::lease_client::LeaseClient<tonic::transport::Channel>,
            lease_revoke,
            req
        );
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
                            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
        request: Request<pb::MoveLeaderRequest>,
    ) -> std::result::Result<Response<pb::MoveLeaderResponse>, Status> {
        let req = request.into_inner();
        let raft = self.raft()?;
        // Not forwarded: etcd requires this to be sent to the leader, because
        // "give leadership to X" is only meaningful as an instruction from
        // whoever currently holds it.
        if !self.node.is_leader() {
            return Err(Status::failed_precondition(
                "etcdserver: not the leader; send MoveLeader to the current leader",
            ));
        }
        raft.transfer_leader(req.target_id).await.map_err(Status::from)?;
        let revision = self.current_revision()?;
        Ok(Response::new(pb::MoveLeaderResponse { header: self.header(revision) }))
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
        let members = self.node.read(|s| s.members())?;
        // A single member has an empty address book — there was never a
        // membership change to record one — so it reports itself.
        let members = if members.is_empty() {
            vec![pb::Member {
                id: self.node.member_id(),
                name: "nodestore".to_string(),
                is_learner: false,
                ..Default::default()
            }]
        } else {
            members.iter().map(member_to_pb).collect()
        };
        Ok(Response::new(pb::MemberListResponse { header: self.header(revision), members }))
    }

    async fn member_add(
        &self,
        request: Request<pb::MemberAddRequest>,
    ) -> std::result::Result<Response<pb::MemberAddResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(
            self,
            pb::cluster_client::ClusterClient<tonic::transport::Channel>,
            member_add,
            req
        );
        let raft = self.raft()?;

        let peer_url = req
            .peer_ur_ls
            .first()
            .cloned()
            .ok_or_else(|| Status::invalid_argument("etcdserver: peer URLs are required"))?;
        // Deterministic from the URL rather than random: re-adding a member
        // after removing it must not silently create a *second* identity for
        // the same machine, and an operator retrying a request that already
        // succeeded should get the same member back.
        let id = member_id_for(&peer_url);

        // New members join as learners. A learner receives the log but is not
        // counted for quorum, so catching up a fresh replica — which may mean
        // shipping it an entire snapshot — cannot stall the cluster it is
        // joining. Promotion to voter is a separate, explicit step.
        let mut change = raft::eraftpb::ConfChangeSingle::default();
        change.change_type = raft::eraftpb::ConfChangeType::AddLearnerNode;
        change.node_id = id;
        let mut cc = raft::eraftpb::ConfChangeV2::default();
        cc.mut_changes().push(change);

        let member = crate::command::Member {
            id,
            peer_url: peer_url.clone(),
            client_url: String::new(),
            name: format!("member-{id}"),
            is_learner: true,
        };
        raft.propose_conf_change(cc, &Command::SetMember(member.clone()))
            .await
            .map_err(Status::from)?;

        let revision = self.current_revision()?;
        let members = self.node.read(|s| s.members())?;
        Ok(Response::new(pb::MemberAddResponse {
            header: self.header(revision),
            member: Some(member_to_pb(&member)),
            members: members.iter().map(member_to_pb).collect(),
        }))
    }

    async fn member_remove(
        &self,
        request: Request<pb::MemberRemoveRequest>,
    ) -> std::result::Result<Response<pb::MemberRemoveResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(
            self,
            pb::cluster_client::ClusterClient<tonic::transport::Channel>,
            member_remove,
            req
        );
        let raft = self.raft()?;

        // Removing the leader would depose it mid-change. etcd refuses this
        // too; the operator transfers leadership first.
        if req.id == raft.member_id() {
            return Err(Status::failed_precondition(
                "nodestore: this member is the leader — transfer leadership before removing it",
            ));
        }

        let mut change = raft::eraftpb::ConfChangeSingle::default();
        change.change_type = raft::eraftpb::ConfChangeType::RemoveNode;
        change.node_id = req.id;
        let mut cc = raft::eraftpb::ConfChangeV2::default();
        cc.mut_changes().push(change);

        raft.propose_conf_change(cc, &Command::RemoveMember { id: req.id })
            .await
            .map_err(Status::from)?;

        let revision = self.current_revision()?;
        let members = self.node.read(|s| s.members())?;
        Ok(Response::new(pb::MemberRemoveResponse {
            header: self.header(revision),
            members: members.iter().map(member_to_pb).collect(),
        }))
    }

    async fn member_update(
        &self,
        request: Request<pb::MemberUpdateRequest>,
    ) -> std::result::Result<Response<pb::MemberUpdateResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(
            self,
            pb::cluster_client::ClusterClient<tonic::transport::Channel>,
            member_update,
            req
        );
        let _ = self.raft()?;

        let peer_url = req
            .peer_ur_ls
            .first()
            .cloned()
            .ok_or_else(|| Status::invalid_argument("etcdserver: peer URLs are required"))?;
        let existing = self
            .node
            .read(|s| s.member(req.id))?
            .ok_or_else(|| Status::not_found("etcdserver: member not found"))?;

        // An address change is state, not membership: raft's own view of who
        // is in the cluster does not change, so this needs no conf change.
        self.node
            .propose(Command::SetMember(crate::command::Member {
                peer_url,
                ..existing
            }))
            .await?;

        let revision = self.current_revision()?;
        let members = self.node.read(|s| s.members())?;
        Ok(Response::new(pb::MemberUpdateResponse {
            header: self.header(revision),
            members: members.iter().map(member_to_pb).collect(),
        }))
    }

    async fn member_promote(
        &self,
        request: Request<pb::MemberPromoteRequest>,
    ) -> std::result::Result<Response<pb::MemberPromoteResponse>, Status> {
        let req = request.into_inner();
        forward_if_follower!(
            self,
            pb::cluster_client::ClusterClient<tonic::transport::Channel>,
            member_promote,
            req
        );
        let raft = self.raft()?;

        let existing = self
            .node
            .read(|s| s.member(req.id))?
            .ok_or_else(|| Status::not_found("etcdserver: member not found"))?;

        let mut change = raft::eraftpb::ConfChangeSingle::default();
        change.change_type = raft::eraftpb::ConfChangeType::AddNode;
        change.node_id = req.id;
        let mut cc = raft::eraftpb::ConfChangeV2::default();
        cc.mut_changes().push(change);

        raft.propose_conf_change(
            cc,
            &Command::SetMember(crate::command::Member { is_learner: false, ..existing }),
        )
        .await
        .map_err(Status::from)?;

        let revision = self.current_revision()?;
        let members = self.node.read(|s| s.members())?;
        Ok(Response::new(pb::MemberPromoteResponse {
            header: self.header(revision),
            members: members.iter().map(member_to_pb).collect(),
        }))
    }
}

fn member_to_pb(m: &crate::command::Member) -> pb::Member {
    pb::Member {
        id: m.id,
        name: m.name.clone(),
        peer_ur_ls: if m.peer_url.is_empty() { Vec::new() } else { vec![m.peer_url.clone()] },
        client_ur_ls: if m.client_url.is_empty() {
            Vec::new()
        } else {
            vec![m.client_url.clone()]
        },
        is_learner: m.is_learner,
        ..Default::default()
    }
}

/// A member id derived from its peer URL.
///
/// Deterministic on purpose. Random ids would mean an operator who retried a
/// MemberAdd that had in fact succeeded ended up with two members pointing at
/// one machine — and a cluster whose quorum arithmetic counts a replica that
/// does not exist is one that cannot make progress after a single real
/// failure. FNV-1a, masked into the positive range, since raft treats 0 as
/// "no member".
fn member_id_for(peer_url: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in peer_url.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash & 0x7fff_ffff_ffff_ffff).max(1)
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

#[cfg(test)]
mod member_id_tests {
    use super::member_id_for;

    #[test]
    fn the_same_peer_url_always_yields_the_same_id() {
        // The property the whole scheme rests on: an operator retrying a
        // MemberAdd that had in fact succeeded gets the same member back,
        // rather than a second one pointing at the same machine.
        let a = member_id_for("http://10.0.0.2:2380");
        let b = member_id_for("http://10.0.0.2:2380");
        assert_eq!(a, b);
    }

    #[test]
    fn different_peers_get_different_ids() {
        assert_ne!(member_id_for("http://10.0.0.2:2380"), member_id_for("http://10.0.0.3:2380"));
        assert_ne!(member_id_for("http://10.0.0.2:2380"), member_id_for("http://10.0.0.2:2381"));
    }

    #[test]
    fn ids_are_positive_and_never_zero() {
        // Raft reads 0 as "no member", so a member with that id would be
        // indistinguishable from "there is no leader".
        for url in ["", "http://a", "http://[::1]:2380", &"x".repeat(500)] {
            let id = member_id_for(url);
            assert!(id > 0, "{url:?} produced {id}");
            assert!(id <= 0x7fff_ffff_ffff_ffff);
        }
    }
}
