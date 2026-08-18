//! The scheduling queue: which pod gets tried next, and when a stuck pod gets
//! tried again.
//!
//! # Three containers, not one
//!
//!   * **active** — ready to schedule, ordered by the QueueSort plugin.
//!     `pop()` takes from here.
//!   * **backoff** — failed a cycle recently and is serving a short penalty.
//!     Ordered by expiry, not priority.
//!   * **unschedulable** — a *map*, not a queue. Pods that were rejected and
//!     are waiting for a specific cluster change. Nothing polls it; pods leave
//!     only when an event says they should.
//!
//! That last point is the design. A pod that cannot run costs nothing while it
//! waits, because nothing retries it on a timer — it is woken by an event a
//! plugin declared could unblock it. See `hints.rs` for how that decision is
//! made and why an incomplete declaration is dangerous.
//!
//! # The in-flight problem
//!
//! A scheduling cycle takes real time. An event arriving *during* one is,
//! naively, lost: the pod is not in `unschedulable` yet, so a sweep does not
//! see it, and by the time it lands there the event is gone. The pod then
//! waits for the next event that happens to match — which, if the cluster has
//! gone quiet because the thing that changed was the last change, is never.
//!
//! The fix is to record events against the cycles that were running when they
//! arrived. `in_flight` holds a marker per popped pod and a timeline of events
//! interleaved with those markers; when a pod is returned unschedulable, only
//! the events recorded *after its own marker* are replayed for it. This is
//! subtle, it is invisible when it is wrong, and it is tested directly below.
//!
//! # Locking
//!
//! One mutex over the whole queue. Upstream splits it four ways and documents
//! a strict acquisition order because deadlock there is the classic bug; at
//! this project's scale the contention that split buys is not worth the class
//! of bug it introduces, and every operation here is short and non-blocking.
//! If profiling ever shows the single lock hurting, split it in upstream's
//! order — `queue > active > backoff > nominator` — and not otherwise.

pub mod backoff;
pub mod hints;

use crate::cache::PodInfo;
use crate::events::ClusterEvent;
use crate::framework::status::Status;
use crate::framework::ChangedObject;
use backoff::BackoffQueue;
use hints::{HintRegistry, RequeueDecision};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio::sync::Notify;

/// How long a pod may sit in `unschedulable` before it is retried regardless.
///
/// This is the safety net for an incomplete QueueingHint, and nothing else.
/// In a correct implementation it never fires — which is why firing is logged
/// at `warn` with the plugins responsible (see [`SchedulingQueue::flush_timed_out`]).
pub const DEFAULT_MAX_IN_UNSCHEDULABLE: Duration = Duration::from_secs(300);

/// A pod waiting for something to change, and what it is waiting on.
struct Unschedulable {
    pod: Arc<PodInfo>,
    /// Plugins that rejected it with `Unschedulable` — retry after backoff.
    unschedulable_plugins: Vec<&'static str>,
    /// Plugins that rejected it with `Pending` — retry immediately.
    pending_plugins: Vec<&'static str>,
    /// Parked by `PreEnqueue` rather than by a scheduling cycle.
    ///
    /// The two are woken completely differently and conflating them strands
    /// pods. A cycle-rejected pod is woken by a cluster event some plugin
    /// subscribed to. A PreEnqueue-rejected pod — a gated one — is woken by
    /// its own spec changing, which is not a cluster event about anything
    /// else, and which `update()` is the only thing that sees.
    held_by_pre_enqueue: bool,
    since: Instant,
}

/// One entry in the in-flight timeline.
enum InFlight {
    /// A cycle started for this pod.
    PodMarker(String),
    /// An event arrived while at least one cycle was running.
    Event {
        event: ClusterEvent,
        old: Option<ChangedObject>,
        new: Option<ChangedObject>,
    },
}

#[derive(Default)]
struct Inner {
    /// Ordered by the QueueSort plugin. A Vec kept sorted rather than a
    /// BinaryHeap because the ordering lives in a plugin, not in `Ord`, and
    /// wrapping every pod to borrow a comparator into a heap costs more than
    /// the insert it saves at these sizes.
    active: Vec<Arc<PodInfo>>,
    unschedulable: HashMap<String, Unschedulable>,
    in_flight: Vec<InFlight>,
    /// Pods currently mid-cycle. Used to decide whether events even need
    /// recording — with nothing in flight the timeline stays empty.
    in_flight_uids: Vec<String>,
    /// Counted, so a stall shows up as a number rather than as folklore.
    rescued_by_timeout: u64,
}

/// Ordering comparator, supplied by the QueueSort plugin.
pub type LessFn = Arc<dyn Fn(&PodInfo, &PodInfo) -> bool + Send + Sync>;

/// Admission check, supplied by the PreEnqueue plugins.
pub type PreEnqueueFn = Arc<dyn Fn(&PodInfo) -> Status + Send + Sync>;

pub struct SchedulingQueue {
    inner: Mutex<Inner>,
    backoff: Mutex<BackoffQueue>,
    hints: HintRegistry,
    less: LessFn,
    pre_enqueue: PreEnqueueFn,
    max_in_unschedulable: Duration,
    /// Woken whenever something becomes available, so `pop` never polls.
    notify: Notify,
}

impl SchedulingQueue {
    pub fn new(
        hints: HintRegistry,
        less: LessFn,
        pre_enqueue: PreEnqueueFn,
        backoff: BackoffQueue,
        max_in_unschedulable: Duration,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            backoff: Mutex::new(backoff),
            hints,
            less,
            pre_enqueue,
            max_in_unschedulable,
            notify: Notify::new(),
        }
    }

    /// Offer a pod to the queue. Rejected by PreEnqueue means it is held
    /// without ever being counted as a scheduling failure — see
    /// `SchedulingGates`.
    pub fn add(&self, pod: Arc<PodInfo>) {
        if !(self.pre_enqueue)(&pod).is_success() {
            let mut inner = self.inner.lock().unwrap();
            inner.unschedulable.insert(
                pod.uid.clone(),
                Unschedulable {
                    pod,
                    unschedulable_plugins: Vec::new(),
                    pending_plugins: Vec::new(),
                    held_by_pre_enqueue: true,
                    since: Instant::now(),
                },
            );
            return;
        }
        self.push_active(pod);
    }

    /// A pod this queue already knows about was edited.
    ///
    /// # Why this is not just `add`
    ///
    /// Treating every pod update as an arrival is a hot loop, and `report.rs`
    /// closes it: a failed cycle patches the pod's status to say why, the
    /// apiserver emits an update for that patch, `add` puts the pod straight
    /// back into the active queue skipping its backoff, the cycle fails
    /// again, and it patches again — at full speed, forever, hammering the
    /// apiserver on behalf of a pod that simply does not fit.
    ///
    /// So an update replaces the stored object *where the pod already is* and
    /// leaves its position alone. A pod being edited does not by itself make
    /// it schedulable; only a cluster event a plugin subscribed to does, and
    /// that arrives through [`Self::move_all_to_active_or_backoff`].
    ///
    /// It also carries forward the two fields the watch layer cannot know,
    /// because they belong to the queue rather than to the API object:
    /// `queued_at` (fairness — resetting it on every edit starves exactly the
    /// pods being retried most) and `attempts` (backoff — resetting it pins
    /// the delay at its 1s floor).
    pub fn update(&self, pod: Arc<PodInfo>) {
        {
            let mut inner = self.inner.lock().unwrap();

            if let Some(entry) = inner.unschedulable.get_mut(&pod.uid) {
                let merged = carry_queue_state(&entry.pod, &pod);

                // A pod held by PreEnqueue is held by its own spec, so its
                // own spec changing is the only thing that can release it —
                // and that arrives here, as an update, not as a cluster event
                // about something else. Without this a gated pod stays parked
                // until the five-minute safety net, however promptly its gate
                // is removed.
                //
                // Deliberately narrow: only pods PreEnqueue is holding are
                // re-evaluated. Re-activating a *cycle*-rejected pod on every
                // edit is the hot loop this method exists to prevent, and
                // every failed pod is edited, because being told why it failed
                // is an edit.
                if entry.held_by_pre_enqueue && (self.pre_enqueue)(&merged).is_success() {
                    inner.unschedulable.remove(&pod.uid);
                    drop(inner);
                    self.push_active(merged);
                    return;
                }

                entry.pod = merged;
                return;
            }
            if let Some(slot) = inner.active.iter_mut().find(|p| p.uid == pod.uid) {
                *slot = carry_queue_state(slot, &pod);
                return;
            }
        }

        // Serving backoff: replace the payload while preserving the exact
        // expiry. Pod spec is scheduling input, so stale labels, resources,
        // affinity, or PVC references can produce a decision about an object
        // that no longer exists. BackoffQueue owns the deadline and performs
        // the replacement without restarting it.
        if self.backoff.lock().unwrap().update(&pod) {
            return;
        }

        self.add(pod);
    }

    fn push_active(&self, pod: Arc<PodInfo>) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active.iter().any(|p| p.uid == pod.uid) {
                return;
            }
            let idx = inner
                .active
                .iter()
                .position(|existing| (self.less)(&pod, existing))
                .unwrap_or(inner.active.len());
            inner.active.insert(idx, pod);
        }
        self.notify.notify_one();
    }

    /// Take the next pod to schedule, waiting until one is available.
    ///
    /// Also drains the backoff queue, so a pod whose penalty expired while
    /// nothing else was happening is picked up without a separate sweep.
    pub async fn pop(&self) -> Arc<PodInfo> {
        loop {
            self.drain_expired_backoff();
            {
                let mut inner = self.inner.lock().unwrap();
                if !inner.active.is_empty() {
                    let pod = inner.active.remove(0);
                    inner.in_flight_uids.push(pod.uid.clone());
                    inner.in_flight.push(InFlight::PodMarker(pod.uid.clone()));
                    return pod;
                }
            }
            // Wait for either an arrival or the earliest backoff expiry —
            // never a fixed tick. This is the 1Hz flush upstream runs and this
            // implementation does not; see backoff.rs.
            match self.next_backoff_expiry() {
                Some(at) => {
                    let _ = tokio::time::timeout(
                        at.saturating_duration_since(Instant::now()),
                        self.notify.notified(),
                    )
                    .await;
                }
                None => self.notify.notified().await,
            }
        }
    }

    fn drain_expired_backoff(&self) {
        let ready = {
            let mut b = self.backoff.lock().unwrap();
            b.pop_expired(Instant::now())
        };
        for pod in ready {
            self.push_active(pod);
        }
    }

    pub fn next_backoff_expiry(&self) -> Option<Instant> {
        self.backoff.lock().unwrap().next_expiry()
    }

    /// A cycle finished. Drops the pod's marker and garbage-collects events no
    /// earlier marker still needs.
    pub fn done(&self, uid: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.in_flight_uids.retain(|u| u != uid);
        inner.in_flight.retain(|e| !matches!(e, InFlight::PodMarker(u) if u == uid));

        // Events ahead of the first surviving marker can never be replayed for
        // anyone, so they are dead weight. Without this the timeline grows
        // without bound on a busy cluster.
        if inner.in_flight_uids.is_empty() {
            inner.in_flight.clear();
            return;
        }
        let first_marker = inner
            .in_flight
            .iter()
            .position(|e| matches!(e, InFlight::PodMarker(_)))
            .unwrap_or(0);
        inner.in_flight.drain(..first_marker);
    }

    /// A cycle rejected this pod. Replays anything that happened while it was
    /// running before parking it.
    pub fn add_unschedulable(
        &self,
        pod: Arc<PodInfo>,
        unschedulable_plugins: Vec<&'static str>,
        pending_plugins: Vec<&'static str>,
    ) {
        // Charge the pod for the cycle it just spent. This is the *only*
        // place `attempts` moves, because this is the only place a cycle is
        // known to have been wasted — and without it every requeue would be
        // priced as a first attempt, so `backoff_duration` would return the
        // 1s initial delay forever and a permanently unschedulable pod would
        // re-enter the active queue once a second indefinitely. That is a
        // busy-loop wearing a backoff queue's clothing, and it would only
        // show up as unexplained CPU on a cluster with a stuck pod.
        let pod = {
            let mut p = (*pod).clone();
            p.attempts = p.attempts.saturating_add(1);
            Arc::new(p)
        };

        // The in-flight replay. Without this the pod waits for the *next*
        // matching event, which on a quiet cluster may never come.
        let replay: Vec<(ClusterEvent, Option<ChangedObject>, Option<ChangedObject>)> = {
            let inner = self.inner.lock().unwrap();
            let after = inner
                .in_flight
                .iter()
                .position(|e| matches!(e, InFlight::PodMarker(u) if *u == pod.uid))
                .map(|i| i + 1)
                .unwrap_or(inner.in_flight.len());
            inner.in_flight[after..]
                .iter()
                .filter_map(|e| match e {
                    InFlight::Event { event, old, new } => {
                        Some((*event, old.clone(), new.clone()))
                    }
                    InFlight::PodMarker(_) => None,
                })
                .collect()
        };

        for (event, old, new) in &replay {
            match self.hints.decide(
                &pod,
                &unschedulable_plugins,
                &pending_plugins,
                event,
                old.as_ref(),
                new.as_ref(),
            ) {
                RequeueDecision::Immediately => {
                    self.push_active(pod);
                    return;
                }
                RequeueDecision::AfterBackoff => {
                    self.push_backoff(pod);
                    return;
                }
                RequeueDecision::Skip => {}
            }
        }

        let mut inner = self.inner.lock().unwrap();
        inner.unschedulable.insert(
            pod.uid.clone(),
            Unschedulable {
                pod,
                unschedulable_plugins,
                pending_plugins,
                held_by_pre_enqueue: false,
                since: Instant::now(),
            },
        );
    }

    /// For a cycle that failed for a reason no plugin can explain — a bind
    /// RPC error, an internal plugin bug — straight to the backoff timer,
    /// not through `add_unschedulable`'s event-hint matching. With no
    /// plugin names to record, `HintRegistry::decide` returns `Skip` for
    /// every event, including the one that actually freed whatever this
    /// pod needs (`AssignedPod` delete) — `add_unschedulable` would strand
    /// it until the 5-minute safety net instead of the real retry. A
    /// timer-based requeue needs no plugin cooperation to fire.
    pub fn requeue_after_failure(&self, pod: Arc<PodInfo>) {
        // Same attempts accounting `add_unschedulable` documents: without
        // it every requeue prices as a first attempt and `backoff_duration`
        // never grows past its 1s floor.
        let pod = {
            let mut p = (*pod).clone();
            p.attempts = p.attempts.saturating_add(1);
            Arc::new(p)
        };
        self.push_backoff(pod);
    }

    fn push_backoff(&self, pod: Arc<PodInfo>) {
        {
            let mut b = self.backoff.lock().unwrap();
            let attempts = pod.attempts;
            b.push(pod, attempts, Instant::now());
        }
        // Wake `pop`, which may be sleeping until a later expiry than the one
        // just pushed.
        self.notify.notify_one();
    }

    /// Something changed in the cluster. Reconsider every parked pod, and
    /// record the event for any cycle currently running.
    pub fn move_all_to_active_or_backoff(
        &self,
        event: ClusterEvent,
        old: Option<ChangedObject>,
        new: Option<ChangedObject>,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.in_flight_uids.is_empty() {
                inner.in_flight.push(InFlight::Event {
                    event,
                    old: old.clone(),
                    new: new.clone(),
                });
            }
        }

        let mut to_active: Vec<Arc<PodInfo>> = Vec::new();
        let mut to_backoff: Vec<Arc<PodInfo>> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let uids: Vec<String> = inner.unschedulable.keys().cloned().collect();
            for uid in uids {
                let Some(entry) = inner.unschedulable.get(&uid) else {
                    continue;
                };
                let decision = self.hints.decide(
                    &entry.pod,
                    &entry.unschedulable_plugins,
                    &entry.pending_plugins,
                    &event,
                    old.as_ref(),
                    new.as_ref(),
                );
                match decision {
                    RequeueDecision::Skip => {}
                    RequeueDecision::Immediately => {
                        if let Some(e) = inner.unschedulable.remove(&uid) {
                            to_active.push(e.pod);
                        }
                    }
                    RequeueDecision::AfterBackoff => {
                        if let Some(e) = inner.unschedulable.remove(&uid) {
                            to_backoff.push(e.pod);
                        }
                    }
                }
            }
        }

        // PreEnqueue runs again on the way back in — a pod whose gate is still
        // set must not reach the active queue just because something else
        // changed.
        for pod in to_active {
            self.add(pod);
        }
        for pod in to_backoff {
            if (self.pre_enqueue)(&pod).is_success() {
                self.push_backoff(pod);
            } else {
                self.add(pod);
            }
        }
    }

    /// The safety net. Retries anything that has been parked too long.
    ///
    /// Every rescue here is a bug report: it means some plugin rejected a pod
    /// and did not subscribe to the change that unblocked it. Hence the `warn`
    /// naming the plugins — in a correct implementation this function moves
    /// nothing, ever.
    pub fn flush_timed_out(&self) {
        let now = Instant::now();
        let expired: Vec<(Arc<PodInfo>, Vec<&'static str>)> = {
            let mut inner = self.inner.lock().unwrap();
            let uids: Vec<String> = inner
                .unschedulable
                .iter()
                .filter(|(_, e)| now.duration_since(e.since) >= self.max_in_unschedulable)
                .map(|(uid, _)| uid.clone())
                .collect();
            uids.into_iter()
                .filter_map(|uid| inner.unschedulable.remove(&uid))
                .map(|e| {
                    let mut blamed = e.unschedulable_plugins.clone();
                    blamed.extend(e.pending_plugins.iter().copied());
                    (e.pod, blamed)
                })
                .collect()
        };

        if expired.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.rescued_by_timeout += expired.len() as u64;
        }
        for (pod, blamed) in expired {
            tracing::warn!(
                pod = %pod.key(),
                plugins = ?blamed,
                "pod was rescued by the unschedulable timeout — some plugin rejected it \
                 without subscribing to the event that unblocked it. This is a bug in that \
                 plugin's events_to_register(), not a normal occurrence."
            );
            self.push_backoff(pod);
        }
    }

    /// When the oldest parked pod will time out, so the run loop can sleep to
    /// it rather than sweeping on a 30-second tick.
    pub fn next_timeout_deadline(&self) -> Option<Instant> {
        let inner = self.inner.lock().unwrap();
        inner
            .unschedulable
            .values()
            .map(|e| e.since + self.max_in_unschedulable)
            .min()
    }

    /// Force pods into the active queue, for a plugin that knows something the
    /// event system cannot express.
    pub fn activate(&self, uids: &[String]) {
        let pods: Vec<Arc<PodInfo>> = {
            let mut inner = self.inner.lock().unwrap();
            uids.iter().filter_map(|uid| inner.unschedulable.remove(uid)).map(|e| e.pod).collect()
        };
        for pod in pods {
            self.add(pod);
        }
    }

    /// Drop a pod entirely — it was deleted.
    pub fn remove(&self, uid: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.active.retain(|p| p.uid != uid);
            inner.unschedulable.remove(uid);
        }
        self.backoff.lock().unwrap().remove(uid);
    }

    pub fn active_len(&self) -> usize {
        self.inner.lock().unwrap().active.len()
    }

    pub fn unschedulable_len(&self) -> usize {
        self.inner.lock().unwrap().unschedulable.len()
    }

    pub fn backoff_len(&self) -> usize {
        self.backoff.lock().unwrap().len()
    }

    /// How many pods the safety net has had to rescue. Should be zero forever.
    pub fn rescued_by_timeout(&self) -> u64 {
        self.inner.lock().unwrap().rescued_by_timeout
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;

/// Merge an updated pod object with the queue-owned fields of the one it
/// replaces. See [`SchedulingQueue::update`].
fn carry_queue_state(previous: &Arc<PodInfo>, updated: &Arc<PodInfo>) -> Arc<PodInfo> {
    let mut merged = (**updated).clone();
    merged.queued_at = previous.queued_at;
    merged.attempts = previous.attempts;
    Arc::new(merged)
}
