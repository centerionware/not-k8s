//! The raft driver: one task, one `RawNode`, one Ready loop.
//!
//! # Why everything funnels through a single task
//!
//! `raft::RawNode` is not thread-safe, and more importantly the *order* in
//! which its steps happen is part of raft's correctness. One owning task with
//! a request channel makes that order structural rather than something each
//! caller has to remember.
//!
//! # The Ready loop's order is not stylistic
//!
//! Each pass does these in this sequence, and swapping two of them breaks a
//! specific guarantee:
//!
//!   1. **Send the pre-persist messages.** Raft has already decided these are
//!      safe to send before this node's own disk write.
//!   2. **Apply a snapshot, if one arrived.** It supersedes everything this
//!      node knew, so it has to land before any entry is applied on top.
//!   3. **Apply committed entries.** These are durable on a quorum already.
//!   4. **Persist new entries, then the hard state.** Entries first: a hard
//!      state advertising a commit index whose entries are not on disk would,
//!      after a crash, claim durability for entries that are gone. This is the
//!      single ordering mistake most likely to lose a committed write.
//!   5. **Send the post-persist messages** — the ones that were only safe to
//!      send once this node's own log was durable, e.g. a vote.
//!
//! # Losing leadership fails proposals
//!
//! Entries a deposed leader proposed but never committed will be overwritten
//! by the next leader. Their callers are told so, immediately, rather than
//! waiting for a timeout that would hold an apiserver worker the whole time.

use crate::command::{Command, Member};
use crate::consensus::Node;
use crate::encode::{decode_entry, decode_snapshot, encode_entry, encode_snapshot};
use crate::error::{Error, Result};
use crate::replication::log::{snapshot_with, RaftLog};
use crate::replication::logging::raft_logger;
use crate::replication::proposals::{ProposalResult, ProposalTracker};
use crate::replication::transport::{ClusterState, Transport};
use crate::store::Applied;
use raft::eraftpb::{
    ConfChange, ConfChangeSingle, ConfChangeV2, Entry, EntryType, Message, Snapshot,
};
use raft::{Config as RaftConfig, RawNode, StateRole};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// How often raft's internal clock advances. Election and heartbeat timeouts
/// are expressed in these ticks.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// How long a proposal waits before giving up.
///
/// Long enough to survive an election (which is bounded by the election
/// timeout plus a term's worth of campaigning), short enough that a caller
/// blocked on it is not blocked forever. A write that times out has *not*
/// necessarily failed — it may commit later — which is why the error says so
/// rather than claiming the write did not happen.
const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Entries kept in the log beyond the last snapshot before compacting.
const LOG_COMPACT_THRESHOLD: u64 = 5_000;

/// What the driver accepts from the outside.
enum Request {
    Propose { data: Vec<u8>, id: u64 },
    ConfChange { cc: ConfChangeV2, context: Vec<u8>, id: u64 },
    Step(Message),
    TransferLeader { to: u64, done: oneshot::Sender<Result<()>> },
    Campaign { done: oneshot::Sender<Result<()>> },
}

/// Handle to a running driver. Cloneable and cheap.
#[derive(Clone)]
pub struct RaftHandle {
    tx: mpsc::Sender<Request>,
    proposals: Arc<ProposalTracker>,
    pub state: Arc<ClusterState>,
    member_id: u64,
}

impl RaftHandle {
    pub fn member_id(&self) -> u64 {
        self.member_id
    }

    pub fn is_leader(&self) -> bool {
        self.state.is_leader()
    }

    pub fn leader_id(&self) -> Option<u64> {
        self.state.leader()
    }

    pub fn term(&self) -> u64 {
        self.state.term.load(Ordering::Relaxed)
    }

    pub fn applied_index(&self) -> u64 {
        self.state.applied_index.load(Ordering::Relaxed)
    }

    /// Propose a command and wait for it to be applied here.
    pub async fn propose(&self, cmd: &Command) -> Result<Applied> {
        let (id, rx) = self.proposals.register();
        let data = encode_entry(id, cmd);
        if self.tx.send(Request::Propose { data, id }).await.is_err() {
            self.proposals.forget(id);
            return Err(Error::Unavailable("the raft driver has stopped".to_string()));
        }
        self.await_proposal(id, rx).await
    }

    /// Propose a membership change together with the address-book update that
    /// must accompany it.
    ///
    /// The two travel as one entry — the conf change with the `SetMember` (or
    /// `RemoveMember`) command in its context — because a membership that
    /// committed without its address, or an address without its membership,
    /// leaves the cluster unable to talk to a member it believes exists.
    pub async fn propose_conf_change(
        &self,
        cc: ConfChangeV2,
        cmd: &Command,
    ) -> Result<Applied> {
        let (id, rx) = self.proposals.register();
        let context = encode_entry(id, cmd);
        if self.tx.send(Request::ConfChange { cc, context, id }).await.is_err() {
            self.proposals.forget(id);
            return Err(Error::Unavailable("the raft driver has stopped".to_string()));
        }
        self.await_proposal(id, rx).await
    }

    async fn await_proposal(
        &self,
        id: u64,
        rx: oneshot::Receiver<ProposalResult>,
    ) -> Result<Applied> {
        match tokio::time::timeout(PROPOSAL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.proposals.forget(id);
                Err(Error::Unavailable("the raft driver dropped this proposal".to_string()))
            }
            Err(_) => {
                self.proposals.forget(id);
                // Deliberately not "the write failed": a timed-out proposal
                // may still commit. Saying otherwise would invite a caller to
                // retry a write that is about to happen anyway.
                Err(Error::Unavailable(
                    "timed out waiting for this write to be committed by the cluster; it may still \
                     be applied — re-read before assuming it was not"
                        .to_string(),
                ))
            }
        }
    }

    /// Hand an inbound raft message to the driver.
    pub async fn step(&self, msg: Message) {
        let _ = self.tx.send(Request::Step(msg)).await;
    }

    pub async fn transfer_leader(&self, to: u64) -> Result<()> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Request::TransferLeader { to, done })
            .await
            .map_err(|_| Error::Unavailable("the raft driver has stopped".to_string()))?;
        rx.await.map_err(|_| Error::Unavailable("the raft driver has stopped".to_string()))?
    }

    /// Force an election. Used only to bootstrap a single-member cluster,
    /// which would otherwise wait out an election timeout with nobody to
    /// campaign against.
    pub async fn campaign(&self) -> Result<()> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Request::Campaign { done })
            .await
            .map_err(|_| Error::Unavailable("the raft driver has stopped".to_string()))?;
        rx.await.map_err(|_| Error::Unavailable("the raft driver has stopped".to_string()))?
    }
}

pub struct Driver {
    raw: RawNode<RaftLog>,
    log: RaftLog,
    node: Arc<Node>,
    transport: Arc<Transport>,
    proposals: Arc<ProposalTracker>,
    state: Arc<ClusterState>,
    rx: mpsc::Receiver<Request>,
    last_role: StateRole,
}

/// Start the driver. Returns a handle; the loop runs on its own task.
pub fn start(
    member_id: u64,
    peers: Vec<Member>,
    log: RaftLog,
    node: Arc<Node>,
    transport: Arc<Transport>,
    election_ticks: usize,
    heartbeat_ticks: usize,
) -> Result<RaftHandle> {
    let applied = node.read(|s| s.applied_index())?;

    let cfg = RaftConfig {
        id: member_id,
        election_tick: election_ticks,
        heartbeat_tick: heartbeat_ticks,
        // Telling raft where the state machine already is stops it
        // re-delivering entries this node applied before its last restart —
        // which, since applying is not idempotent for a compare-and-swap,
        // would produce a different result the second time.
        applied,
        // Pre-vote: a member that was partitioned away rejoins without
        // forcing an election, because it must win a pre-vote before bumping
        // the term. Without it, a flapping network link deposes healthy
        // leaders repeatedly.
        pre_vote: true,
        // Cap how much a single append can carry, so catching up a far-behind
        // follower cannot produce a message too large to send.
        max_size_per_msg: 1024 * 1024,
        max_inflight_msgs: 256,
        ..Default::default()
    };
    cfg.validate().map_err(|e| Error::InvalidRequest(format!("invalid raft config: {e}")))?;

    let mut raw = RawNode::new(&cfg, log.clone(), &raft_logger())
        .map_err(|e| Error::Unavailable(format!("starting raft: {e}")))?;

    // A brand-new cluster has an empty conf state, which raft reads as "I am
    // in no cluster" — it will never campaign and never accept a proposal.
    // Seeding it from the configured membership is what bootstraps it.
    let bootstrap = raw.raft.prs().conf().voters().ids().is_empty();
    if bootstrap && !peers.is_empty() {
        let voters: Vec<u64> = peers.iter().filter(|m| !m.is_learner).map(|m| m.id).collect();
        let learners: Vec<u64> = peers.iter().filter(|m| m.is_learner).map(|m| m.id).collect();
        let mut cs = raft::eraftpb::ConfState::default();
        cs.voters = voters.clone();
        cs.learners = learners.clone();
        info!(?voters, ?learners, "bootstrapping cluster membership");
        log.set_conf_state(&cs)?;
        // Rebuild with the seeded membership rather than mutating the running
        // node: raft derives its progress tracker from the conf state at
        // construction.
        raw = RawNode::new(&cfg, log.clone(), &raft_logger())
            .map_err(|e| Error::Unavailable(format!("restarting raft after bootstrap: {e}")))?;
    }

    transport.set_peers(&peers);

    let (tx, rx) = mpsc::channel(1024);
    let proposals = Arc::new(ProposalTracker::new());
    let state = Arc::new(ClusterState::default());
    state.applied_index.store(applied, Ordering::Relaxed);

    let driver = Driver {
        raw,
        log,
        node,
        transport,
        proposals: Arc::clone(&proposals),
        state: Arc::clone(&state),
        rx,
        last_role: StateRole::Follower,
    };
    tokio::spawn(driver.run());

    Ok(RaftHandle { tx, proposals, state, member_id })
}

impl Driver {
    async fn run(mut self) {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.raw.tick();
                }
                request = self.rx.recv() => {
                    match request {
                        Some(req) => self.handle(req),
                        None => {
                            info!("raft driver stopping: no handles left");
                            self.proposals.fail_all("the datastore is shutting down");
                            return;
                        }
                    }
                }
            }

            if let Err(e) = self.on_ready() {
                // A failure here is a storage failure, not a transient one.
                // Continuing would apply entries on top of state that may not
                // have persisted, so this is loud and the loop keeps running
                // only so the process stays diagnosable.
                error!(error = %e, "raft ready processing failed");
            }
            self.observe_role();
            if let Err(e) = self.maintain_snapshot() {
                warn!(error = %e, "snapshot maintenance failed");
            }
        }
    }

    fn handle(&mut self, req: Request) {
        match req {
            Request::Propose { data, id } => {
                if !self.raw.raft.state.eq(&StateRole::Leader) {
                    self.proposals.complete(
                        id,
                        Err(Error::Unavailable(
                            "this member is not the leader; the write was not proposed".to_string(),
                        )),
                    );
                    return;
                }
                if let Err(e) = self.raw.propose(Vec::new(), data) {
                    self.proposals.complete(id, Err(Error::Unavailable(format!("propose: {e}"))));
                }
            }
            Request::ConfChange { cc, context, id } => {
                if !self.raw.raft.state.eq(&StateRole::Leader) {
                    self.proposals.complete(
                        id,
                        Err(Error::Unavailable(
                            "membership changes must go to the leader".to_string(),
                        )),
                    );
                    return;
                }
                if let Err(e) = self.raw.propose_conf_change(context, cc) {
                    self.proposals
                        .complete(id, Err(Error::Unavailable(format!("propose conf change: {e}"))));
                }
            }
            Request::Step(msg) => {
                if let Err(e) = self.raw.step(msg) {
                    // Stale terms and messages from removed members land here.
                    // Raft rejects them by design; this is not a fault.
                    debug!(error = %e, "raft rejected an inbound message");
                }
            }
            Request::TransferLeader { to, done } => {
                let result = if self.raw.raft.state != StateRole::Leader {
                    Err(Error::Unavailable("only the leader can transfer leadership".to_string()))
                } else {
                    self.raw.transfer_leader(to);
                    Ok(())
                };
                let _ = done.send(result);
            }
            Request::Campaign { done } => {
                let result = self
                    .raw
                    .campaign()
                    .map_err(|e| Error::Unavailable(format!("campaign: {e}")));
                let _ = done.send(result);
            }
        }
    }

    /// One pass of the Ready loop. See the module header for why the order is
    /// what it is.
    fn on_ready(&mut self) -> Result<()> {
        if !self.raw.has_ready() {
            return Ok(());
        }
        let mut ready = self.raw.ready();

        if !ready.messages().is_empty() {
            self.transport.send_all(ready.take_messages());
        }

        if *ready.snapshot() != Snapshot::default() {
            let snapshot = ready.snapshot().clone();
            self.install_received_snapshot(snapshot)?;
        }

        let committed = ready.take_committed_entries();
        self.apply_entries(committed)?;

        if !ready.entries().is_empty() {
            // Entries before hard state: a hard state advertising a commit
            // index whose entries are not on disk would, after a crash, claim
            // durability for entries that are gone.
            self.log.append(ready.entries())?;
        }
        if let Some(hs) = ready.hs() {
            self.log.set_hard_state(hs)?;
        }

        if !ready.persisted_messages().is_empty() {
            self.transport.send_all(ready.take_persisted_messages());
        }

        let mut light = self.raw.advance(ready);
        if light.commit_index().is_some() {
            let hs = self.raw.raft.hard_state();
            self.log.set_hard_state(&hs)?;
        }
        self.transport.send_all(light.take_messages());
        let committed = light.take_committed_entries();
        self.apply_entries(committed)?;
        self.raw.advance_apply();
        Ok(())
    }

    fn apply_entries(&mut self, entries: Vec<Entry>) -> Result<()> {
        for entry in entries {
            let index = entry.index;
            match entry.entry_type {
                EntryType::EntryNormal => {
                    // Raft appends an empty entry when a leader takes office.
                    // It carries no command and there is nothing to apply.
                    if entry.data.is_empty() {
                        self.state.applied_index.store(index, Ordering::Relaxed);
                        continue;
                    }
                    let (proposal_id, cmd) = decode_entry(&entry.data)?;
                    let result = self.node.apply_committed(index, &cmd);
                    if let Err(e) = &result {
                        // The entry is committed cluster-wide; failing to
                        // apply it here means this replica has diverged.
                        error!(index, error = %e, "failed to apply a committed entry");
                    }
                    self.proposals.complete(proposal_id, result);
                }
                EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                    self.apply_conf_change(&entry, index)?;
                }
            }
            self.state.applied_index.store(index, Ordering::Relaxed);
        }
        Ok(())
    }

    fn apply_conf_change(&mut self, entry: &Entry, index: u64) -> Result<()> {
        let cc: ConfChangeV2 = if entry.entry_type == EntryType::EntryConfChange {
            let v1: ConfChange = protobuf::Message::parse_from_bytes(&entry.data)
                .map_err(|e| Error::InvalidRequest(format!("bad conf change: {e}")))?;
            // The old single-change form, widened to the one this code
            // applies. Built by hand rather than through raft's own
            // conversion trait so this does not depend on where that trait
            // happens to be exported from.
            //
            // Reachable only from a log written by something that proposed
            // the v1 form — this crate always proposes v2 — but a committed
            // entry it refused to apply would strand the replica.
            let mut v2 = ConfChangeV2::default();
            let mut single = ConfChangeSingle::default();
            single.change_type = v1.get_change_type();
            single.node_id = v1.get_node_id();
            v2.mut_changes().push(single);
            v2.context = v1.context.clone();
            v2
        } else {
            protobuf::Message::parse_from_bytes(&entry.data)
                .map_err(|e| Error::InvalidRequest(format!("bad conf change v2: {e}")))?
        };

        let conf_state = self
            .raw
            .apply_conf_change(&cc)
            .map_err(|e| Error::Unavailable(format!("applying conf change: {e}")))?;
        self.log.set_conf_state(&conf_state)?;

        // The address-book update rides in the entry's context, so membership
        // and reachability commit together. A member the cluster believes in
        // but cannot address is worse than one it does not know about.
        if !entry.context.is_empty() {
            let (proposal_id, cmd) = decode_entry(&entry.context)?;
            let result = self.node.apply_committed(index, &cmd);
            self.proposals.complete(proposal_id, result);
        }

        match self.node.read(|s| s.members()) {
            Ok(members) => {
                info!(
                    voters = ?conf_state.voters,
                    learners = ?conf_state.learners,
                    "cluster membership changed"
                );
                self.transport.set_peers(&members);
            }
            Err(e) => warn!(error = %e, "could not refresh peers after a membership change"),
        }
        Ok(())
    }

    /// Replace this node's state with a snapshot from the leader.
    fn install_received_snapshot(&mut self, snapshot: Snapshot) -> Result<()> {
        let meta = snapshot.get_metadata().clone();
        info!(index = meta.index, term = meta.term, "restoring from a leader's snapshot");

        let state = decode_snapshot(snapshot.get_data())?;
        self.node.restore_snapshot(&state)?;
        self.log.install_snapshot(snapshot)?;
        self.state.applied_index.store(meta.index, Ordering::Relaxed);

        if let Ok(members) = self.node.read(|s| s.members()) {
            self.transport.set_peers(&members);
        }
        Ok(())
    }

    /// Build a snapshot when raft has asked for one, or when the log has grown
    /// past the point worth keeping.
    fn maintain_snapshot(&mut self) -> Result<()> {
        let requested = self.log.pending_snapshot_request();
        let applied = self.state.applied_index.load(Ordering::Relaxed);
        let first = self.log.snapshot_index()?;

        let needed = requested.is_some() || applied.saturating_sub(first) > LOG_COMPACT_THRESHOLD;
        if !needed || applied == 0 {
            return Ok(());
        }

        let term = match self.raw.raft.raft_log.term(applied) {
            Ok(t) => t,
            // The term for that index is already compacted away, which means a
            // snapshot at least that new exists. Nothing to do.
            Err(_) => return Ok(()),
        };
        let conf_state = self.raw.raft.prs().conf().to_conf_state();
        let state = self.node.read(|s| s.export_snapshot())?;
        let data = encode_snapshot(&state);
        let size = data.len();

        self.log.install_snapshot(snapshot_with(applied, term, conf_state, data))?;
        info!(index = applied, bytes = size, "took a snapshot and compacted the log");
        Ok(())
    }

    /// Publish role changes, and fail proposals a lost leadership invalidated.
    fn observe_role(&mut self) {
        let role = self.raw.raft.state;
        let leader = self.raw.raft.leader_id;
        let term = self.raw.raft.term;

        if role != self.last_role {
            match role {
                StateRole::Leader => info!(term, "became leader"),
                StateRole::Follower => info!(term, leader, "became follower"),
                StateRole::Candidate => info!(term, "campaigning"),
                StateRole::PreCandidate => debug!(term, "pre-campaigning"),
            }
            if self.last_role == StateRole::Leader {
                // Entries proposed here and not yet committed will be
                // overwritten by the next leader. Their callers must be told
                // now, not by a timeout ten seconds from now.
                let failed = self.proposals.fail_all(
                    "leadership was lost before this write committed; it did not happen",
                );
                if failed > 0 {
                    warn!(failed, "failed in-flight proposals after losing leadership");
                }
            }
            self.last_role = role;
        }

        self.state.set_role(role);
        self.state.leader_id.store(leader, Ordering::Relaxed);
        self.state.term.store(term, Ordering::Relaxed);
    }
}
