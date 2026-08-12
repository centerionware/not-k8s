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

/// The term a converted single member's synthetic snapshot is stamped with.
///
/// 1, not 0: raft reserves term 0 for "before anything", and a snapshot at
/// term 0 would compare as older than any real entry.
const SEED_TERM: u64 = 1;

/// Turn a populated single member into the seed of a raft cluster, in place.
///
/// # Why this is needed
///
/// Single-member mode is not raft — `SingleNode` applies commands directly and
/// writes no log. So a member that has been serving a cluster for a month has
/// a full state machine and *no* raft history whatsoever. Point it at a
/// cluster configuration and raft starts at index 0 behind a state machine at
/// index N, which is unrecoverable by replay and is exactly what
/// `reconcile_commit_with_applied` refuses.
///
/// The way out is the one etcd uses for the same shape of problem: treat the
/// existing state as a snapshot. A snapshot is precisely "the state machine as
/// of index N, with no need for the entries that built it", which is a true
/// description of what this member has. Installing one at `applied` makes the
/// log consistent with the state machine, gives the member a real log position
/// to lead from, and — because the snapshot carries the actual data — is what
/// every member added afterwards receives to catch up.
///
/// # Why only for a single-member cluster
///
/// This is refused unless the configured cluster is this member alone, and the
/// reason is a real way to lose the data rather than a formality.
///
/// Raft's election restriction only stops a candidate with a *shorter* log
/// from winning votes it needs from members with longer ones. Configure three
/// members where one has the data and two are empty, and the two empty ones
/// are a majority: they can elect each other with no reference to the member
/// that holds everything, and the new leader then overwrites it with its own
/// empty log. The data would be gone, correctly, by raft's own rules.
///
/// So the supported upgrade is the one that never puts the data in the
/// minority: convert this member into a one-member cluster (it is the only
/// voter, so it always wins and its state is by definition the truth), then
/// grow with `MemberAdd`, one member at a time. Each new member joins a
/// cluster that already has a quorum holding the data, and is caught up by the
/// snapshot this function installed.
fn adopt_existing_state_as_the_cluster_seed(
    member_id: u64,
    peers: &[Member],
    log: &RaftLog,
    node: &Arc<Node>,
    applied: u64,
) -> Result<()> {
    let others: Vec<u64> = peers.iter().map(|m| m.id).filter(|id| *id != member_id).collect();
    if !others.is_empty() {
        return Err(Error::InvalidRequest(format!(
            "this member has {applied} applied entries but no raft log, because it has been \
             running as a single member — and single-member mode keeps no log. It cannot join a \
             cluster that also configures members {others:?}: those start empty, and two empty \
             members are a majority that can elect each other and overwrite everything this one \
             holds. Convert it first by setting NODESTORE_INITIAL_CLUSTER to this member alone \
             ({member_id}=<its peer URL>), start it, and then add the others one at a time with \
             MemberAdd — each is then caught up from this member's data instead of voting it away."
        )));
    }

    let mut cs = raft::eraftpb::ConfState::default();
    cs.voters = vec![member_id];

    // The snapshot carries the real state machine, not just metadata: this is
    // what a member added later is sent, so it has to be the data itself.
    let state = node.read(|s| s.export_snapshot())?;
    let data = crate::encode::encode_snapshot(&state);
    let bytes = data.len();
    log.install_snapshot(snapshot_with(applied, SEED_TERM, cs.clone(), data))?;
    log.set_conf_state(&cs, applied)?;

    // The log now *begins* at `applied`, so raft must be told this member has
    // committed that far. Without it raft starts at commit 0 against a first
    // index of applied+1 and cannot reconcile the two.
    let mut hs = raft::eraftpb::HardState::default();
    hs.term = SEED_TERM;
    hs.commit = applied;
    log.set_hard_state(&hs)?;

    info!(
        index = applied,
        bytes,
        "converted a single member into a one-member cluster by adopting its existing state as the \
         cluster's first snapshot — add the other members with MemberAdd, one at a time"
    );
    Ok(())
}

/// Refuse to start empty under an id a live cluster still has progress for.
///
/// # The crash this prevents
///
/// Found live on a three-member cluster. Member 3's data directory was removed
/// and the member restarted under the same id. It bootstrapped a fresh
/// membership, the leader — which still held `matched = 10` for member 3 from
/// before — sent it a heartbeat carrying that commit index, and raft-rs
/// panicked: `to_commit 10 is out of range [last_index 0]`. The leader had no
/// way to know the member it was talking to was not the one it had been
/// replicating to.
///
/// This is the same hazard etcd forbids structurally by requiring a member to
/// be removed and re-added under a *new* id. `MemberRemove` + `MemberAdd` is
/// exactly that, and is what the error points at.
///
/// # Why a probe rather than a local marker
///
/// Any marker written into the data directory disappears with the data
/// directory, which is the very event being detected. The only party that
/// still knows this member used to exist is the rest of the cluster, so the
/// rest of the cluster is who gets asked.
///
/// A silent probe — nothing reachable, or nothing running — is deliberately
/// permissive: a whole cluster starting at once has every member empty and no
/// leader yet, and that has to keep working.
fn refuse_empty_restart_into_a_live_cluster(
    member_id: u64,
    probe: &crate::replication::transport::ClusterProbe,
) -> Result<()> {
    if !probe.already_running || !probe.voters.contains(&member_id) {
        return Ok(());
    }
    Err(Error::InvalidRequest(format!(
        "this member's data directory is empty, but the cluster is already running and still \
         counts member {member_id} as a voter — so its leader holds a replication position for an \
         id with nothing behind it, and would drive it past the end of its own log. Remove it from \
         the cluster with MemberRemove and add it back with MemberAdd, which issues a new id and \
         makes the leader send a snapshot instead. (Restarting a member with the same id and an \
         empty directory is what etcd forbids for this reason.)"
    )))
}

/// Start the driver. Returns a handle; the loop runs on its own task.
///
/// `probe` is what the peers said before raft was built. It is consulted only
/// when this member has no history of its own, to tell "a new cluster is being
/// created" apart from "this member was emptied under a cluster that is still
/// running" — see `refuse_empty_restart_into_a_live_cluster()`.
pub fn start(
    member_id: u64,
    peers: Vec<Member>,
    log: RaftLog,
    node: Arc<Node>,
    transport: Arc<Transport>,
    election_ticks: usize,
    heartbeat_ticks: usize,
    probe: crate::replication::transport::ClusterProbe,
) -> Result<RaftHandle> {
    let applied = node.read(|s| s.applied_index())?;

    // A single member keeps no raft log at all — it has nobody to convince, so
    // nothing is ever proposed and `raft.db` does not exist. Turning that
    // member into a clustered one therefore starts raft with an empty log
    // behind a state machine that is already at `applied`, which the check
    // below correctly refuses. Seeding the log from the state machine is what
    // makes that conversion possible in place, without discarding the data
    // that is the entire reason to keep the member.
    if applied > 0 && log.last_index_value()? == 0 && log.snapshot_index()? == 0 {
        adopt_existing_state_as_the_cluster_seed(member_id, &peers, &log, &node, applied)?;
    }

    // Before raft sees `applied`: the persisted commit index can legitimately
    // trail it across a restart, because raft does not require the commit
    // index to be durable while entries and the applied index both are.
    // RawNode::new *panics* on that combination, so a member that hit it
    // could never restart, let alone rejoin. See the method's own comment.
    if log.reconcile_commit_with_applied(applied)? {
        info!(applied, "raised the persisted commit index to the applied index on restart");
    }

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
    //
    // But an empty conf state means only "this member has no history", and
    // there are two very different ways to have none: the cluster is being
    // created right now, or this member's data directory was emptied under an
    // id the cluster has been running with. `probe` is what separates them —
    // see refuse_empty_restart_into_a_live_cluster().
    let bootstrap = raw.raft.prs().conf().voters().ids().is_empty();
    if bootstrap {
        refuse_empty_restart_into_a_live_cluster(member_id, &probe)?;
    }
    if bootstrap && probe.already_running && !probe.voters.contains(&member_id) {
        // Added to a running cluster with MemberAdd: the leader already knows
        // about this member and will send it a snapshot. Seeding a membership
        // here would be this member inventing a configuration the cluster
        // never agreed to, so it starts with none and takes the leader's.
        info!("joining a cluster that is already running; waiting for the leader's snapshot");
    } else if bootstrap && !peers.is_empty() {
        let voters: Vec<u64> = peers.iter().filter(|m| !m.is_learner).map(|m| m.id).collect();
        let learners: Vec<u64> = peers.iter().filter(|m| m.is_learner).map(|m| m.id).collect();
        let mut cs = raft::eraftpb::ConfState::default();
        cs.voters = voters.clone();
        cs.learners = learners.clone();
        info!(?voters, ?learners, "bootstrapping cluster membership");
        // Index 0: this is the seed, observed before any entry exists. Any
        // real configuration change later carries a higher index and so wins
        // over both this and a snapshot — see set_conf_state()'s own comment.
        log.set_conf_state(&cs, 0)?;
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
                // Stop, do not continue. raft requires every Ready to reach
                // `advance` before the node processes any further state
                // change, and a failure part-way through leaves one
                // outstanding. Looping would then call tick/step/propose with
                // that Ready still pending, which desynchronises raft's
                // internal state — a far worse failure than the storage error
                // that caused it, and one that would present as a member that
                // is up but quietly wrong.
                //
                // Deliberately no `advance` on the way out either: advancing
                // would acknowledge entries that were not persisted.
                error!(error = %e, "raft ready processing failed; stopping this member");
                let failed = self
                    .proposals
                    .fail_all("the raft driver stopped after a storage failure");
                if failed > 0 {
                    warn!(failed, "failed in-flight proposals while stopping");
                }
                // Every handle now reports Unavailable, which is the truth:
                // this member can no longer replicate. The process stays up so
                // it remains diagnosable, and so a supervisor's restart is a
                // decision rather than a surprise.
                return;
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
        // Recorded at the index of the entry that caused it. This is precisely
        // the case that made the index necessary: a configuration change
        // landing on top of a snapshot is newer than the membership the
        // snapshot carries, and initial_state() has to be able to tell.
        self.log.set_conf_state(&conf_state, entry.index)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::transport::ClusterProbe;

    fn probe(running: bool, voters: Vec<u64>) -> ClusterProbe {
        ClusterProbe { reached_a_peer: !voters.is_empty(), already_running: running, voters }
    }

    /// The crash this exists to prevent: a member restarted empty under an id
    /// the leader still holds a replication position for, which drove raft-rs
    /// past the end of an empty log and panicked.
    #[test]
    fn an_empty_member_may_not_rejoin_a_live_cluster_under_its_old_id() {
        let err = refuse_empty_restart_into_a_live_cluster(3, &probe(true, vec![1, 2, 3]))
            .expect_err("the leader still has progress for member 3");
        let msg = err.to_string();
        assert!(msg.contains("MemberRemove"), "must say how to recover: {msg}");
        assert!(msg.contains("MemberAdd"), "must say how to recover: {msg}");
    }

    /// A whole cluster starting at once has every member empty and no leader
    /// yet. That is the ordinary case and must not be mistaken for the crash
    /// above — nothing has been elected, so nothing holds a position for
    /// anyone.
    #[test]
    fn a_cluster_starting_from_nothing_is_not_refused() {
        refuse_empty_restart_into_a_live_cluster(3, &probe(false, vec![1, 2, 3]))
            .expect("no peer reported a term, so this is a cluster being created");
        refuse_empty_restart_into_a_live_cluster(3, &ClusterProbe::default())
            .expect("no peer answered at all, which proves nothing and must stay permissive");
    }

    /// A member added with MemberAdd is not yet a voter, so the running
    /// cluster holds no position for it — it joins normally and is caught up
    /// by the leader's snapshot.
    #[test]
    fn a_newly_added_member_joins_a_live_cluster_without_complaint() {
        refuse_empty_restart_into_a_live_cluster(4, &probe(true, vec![1, 2, 3]))
            .expect("member 4 is new to this cluster; nothing has a position for it");
    }
}
