//! The inbound half of the peer transport: the gRPC service other members
//! dial.
//!
//! Served on its own port, never on the client port. A raft message carries
//! no authentication and is trusted completely by the member that receives it
//! — a `MsgAppend` is, by construction, "overwrite your log with this" — so
//! the peer port is not somewhere a client should ever be able to reach.

use crate::consensus::Node;
use crate::pb::peer::peer_server::Peer;
use crate::pb::peer::{RaftAck, RaftMessage, StatusReply, StatusRequest};
use crate::replication::driver::RaftHandle;
use raft::eraftpb::Message;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct PeerService {
    handle: RaftHandle,
    node: Arc<Node>,
}

impl PeerService {
    pub fn new(handle: RaftHandle, node: Arc<Node>) -> PeerService {
        PeerService { handle, node }
    }
}

#[tonic::async_trait]
impl Peer for PeerService {
    async fn send(
        &self,
        request: Request<RaftMessage>,
    ) -> std::result::Result<Response<RaftAck>, Status> {
        let payload = request.into_inner().payload;
        let msg: Message = protobuf::Message::parse_from_bytes(&payload)
            .map_err(|e| Status::invalid_argument(format!("undecodable raft message: {e}")))?;

        // Acknowledged as received, not as processed. Raft's own
        // acknowledgements travel as raft messages in the other direction, so
        // making the sender wait for this one to be stepped would add a
        // second, redundant round trip to every append — and would let a busy
        // follower stall the leader's send loop.
        self.handle.step(msg).await;
        Ok(Response::new(RaftAck {}))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> std::result::Result<Response<StatusReply>, Status> {
        let leader_id = self.handle.leader_id().unwrap_or(0);
        let (revision, members) = self
            .node
            .read(|s| Ok((s.revision()?, s.members()?)))
            .map_err(|e| Status::internal(format!("reading cluster state: {e}")))?;

        let leader_client_url = members
            .iter()
            .find(|m| m.id == leader_id)
            .map(|m| m.client_url.clone())
            .unwrap_or_default();

        Ok(Response::new(StatusReply {
            member_id: self.handle.member_id(),
            leader_id,
            term: self.handle.term(),
            applied_index: self.handle.applied_index(),
            revision,
            role: self.handle.state.role_name().to_string(),
            leader_client_url,
            voters: members.iter().filter(|m| !m.is_learner).map(|m| m.id).collect(),
            learners: members.iter().filter(|m| m.is_learner).map(|m| m.id).collect(),
        }))
    }
}
