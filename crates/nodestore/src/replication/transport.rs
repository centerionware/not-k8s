//! Node-to-node transport: raft messages over our own gRPC service.
//!
//! # Lossy on purpose
//!
//! Every send here is best-effort. A message that cannot be delivered — peer
//! down, connection refused, queue full — is dropped and logged, never
//! retried and never buffered indefinitely.
//!
//! That is not a shortcut, it is the interface raft is designed against.
//! Raft assumes an unreliable network in which messages are lost, delayed,
//! duplicated and reordered, and it recovers by *resending state*, not by
//! resending messages: a leader that gets no response re-sends the append
//! from where the follower actually is. A transport that queued messages
//! forever would deliver a burst of stale appends to a peer that has since
//! moved on, and would turn a slow peer into unbounded memory growth on the
//! leader — memory being the thing an edge device has least of.
//!
//! # One task per peer
//!
//! Each peer gets a bounded queue and a task that owns its connection. A peer
//! that is down therefore fills its own queue and blocks nothing else: the
//! raft loop hands messages off without ever awaiting a network write, which
//! matters because that loop is also what answers heartbeats. A transport
//! that could block the driver would make one unreachable follower look like
//! a failed leader to everybody else.

use crate::command::Member;
use crate::pb::peer::peer_client::PeerClient;
use crate::pb::peer::RaftMessage;
use raft::eraftpb::Message;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// How many messages may be in flight to one peer before we start dropping.
///
/// Small deliberately: a deep queue only delays the moment a peer is
/// recognised as unreachable, while making the messages that do arrive
/// staler. Raft's own retry is faster and more correct than our buffering.
const PEER_QUEUE_DEPTH: usize = 64;

/// Live cluster state, published by the driver for anything that needs to
/// know who the leader is without taking the raft loop's lock — the Status
/// RPC, the forwarding path, and the e2e suite.
#[derive(Default)]
pub struct ClusterState {
    pub leader_id: AtomicU64,
    pub term: AtomicU64,
    pub applied_index: AtomicU64,
    /// 0 = follower, 1 = candidate, 2 = leader, 3 = pre-candidate.
    role: AtomicU64,
}

impl ClusterState {
    pub fn set_role(&self, role: raft::StateRole) {
        let v = match role {
            raft::StateRole::Follower => 0,
            raft::StateRole::Candidate => 1,
            raft::StateRole::Leader => 2,
            raft::StateRole::PreCandidate => 3,
        };
        self.role.store(v, Ordering::Relaxed);
    }

    pub fn role_name(&self) -> &'static str {
        match self.role.load(Ordering::Relaxed) {
            1 => "candidate",
            2 => "leader",
            3 => "pre-candidate",
            _ => "follower",
        }
    }

    pub fn is_leader(&self) -> bool {
        self.role.load(Ordering::Relaxed) == 2
    }

    pub fn leader(&self) -> Option<u64> {
        match self.leader_id.load(Ordering::Relaxed) {
            // Raft uses 0 for "no leader known", which is a real state
            // during an election and must not be reported as member 0.
            0 => None,
            id => Some(id),
        }
    }
}

struct PeerHandle {
    url: String,
    tx: mpsc::Sender<Message>,
    /// Dropping this stops the peer's task, so a removed member's connection
    /// does not outlive its membership.
    _shutdown: mpsc::Sender<()>,
}

pub struct Transport {
    self_id: u64,
    peers: Mutex<HashMap<u64, PeerHandle>>,
    /// Peer-domain TLS material. Every peer connection presents this member's
    /// certificate and verifies the other end's against the peer CA — a raft
    /// message is trusted absolutely by whoever receives it, so an
    /// unauthenticated peer link would let anything that can reach the port
    /// append entries to the log.
    tls: Option<crate::tls::Material>,
}

impl Transport {
    pub fn new(self_id: u64, tls: Option<crate::tls::Material>) -> Arc<Transport> {
        Arc::new(Transport { self_id, peers: Mutex::new(HashMap::new()), tls })
    }

    /// Reconcile connections against the replicated address book.
    ///
    /// Called whenever membership changes. A member whose URL changed gets its
    /// task replaced rather than reused — reconnecting is cheap, and sending
    /// to the address a member *used* to have is the kind of bug that only
    /// appears during the failover it would break.
    pub fn set_peers(&self, members: &[Member]) {
        let mut peers = self.peers.lock().expect("transport peers mutex");
        let wanted: HashMap<u64, &Member> =
            members.iter().filter(|m| m.id != self.self_id).map(|m| (m.id, m)).collect();

        peers.retain(|id, handle| match wanted.get(id) {
            Some(m) if m.peer_url == handle.url => true,
            Some(_) => {
                info!(peer = id, "peer address changed; reconnecting");
                false
            }
            None => {
                info!(peer = id, "peer removed from the cluster; dropping its connection");
                false
            }
        });

        for (id, member) in wanted {
            if peers.contains_key(&id) {
                continue;
            }
            let (tx, rx) = mpsc::channel(PEER_QUEUE_DEPTH);
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
            tokio::spawn(peer_task(
                id,
                member.peer_url.clone(),
                rx,
                shutdown_rx,
                self.tls.clone(),
            ));
            info!(peer = id, url = %member.peer_url, "connecting to peer");
            peers.insert(
                id,
                PeerHandle { url: member.peer_url.clone(), tx, _shutdown: shutdown_tx },
            );
        }
    }

    /// Hand a raft message to its peer's queue. Never blocks, never fails.
    pub fn send(&self, msg: Message) {
        let to = msg.to;
        let peers = self.peers.lock().expect("transport peers mutex");
        let Some(peer) = peers.get(&to) else {
            // Normal during startup and immediately after a conf change: raft
            // may address a member whose address book entry has not been
            // applied here yet.
            debug!(peer = to, "no transport for peer; dropping message");
            return;
        };
        if peer.tx.try_send(msg).is_err() {
            // Full queue means this peer is not keeping up. Dropping is what
            // raft expects; the leader will retry from wherever the follower
            // actually is.
            debug!(peer = to, "peer queue full; dropping message");
        }
    }

    pub fn send_all(&self, messages: Vec<Message>) {
        for msg in messages {
            self.send(msg);
        }
    }

    pub fn peer_ids(&self) -> Vec<u64> {
        self.peers.lock().expect("transport peers mutex").keys().copied().collect()
    }
}

/// One peer's connection: dial lazily, send what arrives, reconnect on
/// failure, and exit when the peer is removed.
async fn peer_task(
    id: u64,
    url: String,
    mut rx: mpsc::Receiver<Message>,
    mut shutdown: mpsc::Receiver<()>,
    tls: Option<crate::tls::Material>,
) {
    let mut client: Option<PeerClient<tonic::transport::Channel>> = None;

    loop {
        let msg = tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => m,
                None => return, // transport dropped
            },
            _ = shutdown.recv() => return,
        };

        if client.is_none() {
            match connect_peer(&url, tls.as_ref()).await {
                Ok(c) => {
                    debug!(peer = id, url = %url, "peer connection established");
                    client = Some(c);
                }
                Err(e) => {
                    // Expected while a peer is down or still starting. Raft
                    // retries on its own schedule, so this must not become a
                    // retry loop of its own.
                    debug!(peer = id, url = %url, error = %e, "peer unreachable; dropping message");
                    continue;
                }
            }
        }

        let payload = match protobuf::Message::write_to_bytes(&msg) {
            Ok(bytes) => bytes,
            Err(e) => {
                // Encoding our own outgoing message cannot fail for any
                // reason the network could fix, so this is loud.
                warn!(peer = id, error = %e, "could not encode a raft message");
                continue;
            }
        };

        if let Some(c) = client.as_mut() {
            if let Err(e) = c.send(RaftMessage { payload }).await {
                debug!(peer = id, error = %e, "peer send failed; will reconnect");
                // Force a reconnect: a channel that has failed once will keep
                // failing, and raft's next heartbeat is the retry.
                client = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, url: &str) -> Member {
        Member {
            id,
            peer_url: url.to_string(),
            client_url: String::new(),
            name: format!("n{id}"),
            is_learner: false,
        }
    }

    #[tokio::test]
    async fn peers_exclude_this_node() {
        // Raft never addresses a message to the sender, but a transport that
        // held a connection to itself would deadlock the moment one did.
        let t = Transport::new(1, None);
        t.set_peers(&[member(1, "http://127.0.0.1:1"), member(2, "http://127.0.0.1:2")]);
        assert_eq!(t.peer_ids(), vec![2]);
    }

    #[tokio::test]
    async fn removed_members_lose_their_connection() {
        let t = Transport::new(1, None);
        t.set_peers(&[member(2, "http://127.0.0.1:2"), member(3, "http://127.0.0.1:3")]);
        assert_eq!(t.peer_ids().len(), 2);
        t.set_peers(&[member(2, "http://127.0.0.1:2")]);
        assert_eq!(t.peer_ids(), vec![2]);
    }

    #[tokio::test]
    async fn sending_to_an_unknown_peer_is_a_no_op_rather_than_a_panic() {
        // Happens for real between a conf change being committed and the
        // address book entry being applied here.
        let t = Transport::new(1, None);
        let mut msg = Message::default();
        msg.to = 99;
        t.send(msg); // must not panic
    }

    #[tokio::test]
    async fn sending_to_an_unreachable_peer_does_not_block_the_caller() {
        // The property the whole design rests on: the raft loop hands off
        // without awaiting a network write, so one dead follower cannot make
        // the leader look dead to everyone else.
        let t = Transport::new(1, None);
        t.set_peers(&[member(2, "http://127.0.0.1:1")]); // nothing listening
        let start = std::time::Instant::now();
        for _ in 0..200 {
            let mut msg = Message::default();
            msg.to = 2;
            t.send(msg);
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "send() blocked for {:?}; it must never wait on the network",
            start.elapsed()
        );
    }

    #[test]
    fn an_unknown_leader_is_reported_as_none_not_as_member_zero() {
        // Raft uses 0 for "no leader known", which is a real state during an
        // election. Reporting it as a member id would send forwarded writes
        // to a member that does not exist.
        let state = ClusterState::default();
        assert_eq!(state.leader(), None);
        state.leader_id.store(3, Ordering::Relaxed);
        assert_eq!(state.leader(), Some(3));
    }

    #[test]
    fn role_names_match_what_the_status_rpc_promises() {
        let state = ClusterState::default();
        assert_eq!(state.role_name(), "follower");
        assert!(!state.is_leader());
        state.set_role(raft::StateRole::Leader);
        assert_eq!(state.role_name(), "leader");
        assert!(state.is_leader());
        state.set_role(raft::StateRole::Candidate);
        assert_eq!(state.role_name(), "candidate");
        assert!(!state.is_leader());
    }
}

/// Dial a peer with mutual TLS.
///
/// The certificate is verified against the *host of the peer URL*, which is
/// why tls_sans() in lib.rs includes every address a member advertises: a
/// member reached at an IP needs that IP as a SAN, and the usual symptom of
/// getting this wrong is a handshake failure that looks like the peer being
/// down.
async fn connect_peer(
    url: &str,
    tls: Option<&crate::tls::Material>,
) -> Result<PeerClient<tonic::transport::Channel>, tonic::transport::Error> {
    let endpoint = tonic::transport::Endpoint::from_shared(url.to_string())?;
    let endpoint = match tls {
        Some(material) => {
            let host = crate::host_of(url);
            let cfg = crate::tls::client_tls_config(material, host.as_deref())
                .map_err(|_| bad_tls_material())?;
            endpoint.tls_config(cfg)?
        }
        None => endpoint,
    };
    let channel = endpoint.connect().await?;
    Ok(PeerClient::new(channel))
}

/// tonic::transport::Error has no public constructor, and this path needs to
/// report "the configured TLS material could not be read" through the same
/// Result the dial returns. Producing one by failing a trivially-invalid
/// endpoint parse is ugly but keeps the caller's error handling uniform; the
/// real cause is logged where the material is loaded.
fn bad_tls_material() -> tonic::transport::Error {
    tonic::transport::Endpoint::from_shared("://".to_string())
        .expect_err("'://' is not a valid endpoint")
}
