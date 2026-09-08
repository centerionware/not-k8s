// Server-side bound on how long a bookmark-negotiated `WATCH` may go
// without producing a single frame. This file is `include!`d into
// `server::listener` (like every other file under `listener/`), so the
// prose below is a plain comment, not a `//!` module doc — an inner doc
// comment is invalid in the middle of a module.
//
// Such a connection is promised the cache's periodic progress heartbeat —
// the cacher's driver requests a progress notification from the datastore
// every `cacher::registry::DEFAULT_BOOKMARK_INTERVAL` (60s) and broadcasts
// the resulting `Bookmark` to every subscriber, which the listener forwards
// to exactly the clients that negotiated bookmarks — so one that stays
// silent past this limit has genuinely lost its feed (observed live in e2e:
// a bookmark-negotiated namespace watch delivered nothing — not events, not
// bookmarks — for its whole ~290s lifetime while sibling connections on the
// same cache flowed; nothing server-side ever ended it, and the informer
// stayed stale until the *client's* own `timeoutSeconds` finally made it
// relist, which is far past the e2e harness deadlines).
//
// Earlier attempts to bound this from *inside* the response body (a
// `timeout`/`map_while` idle cap on the body stream) did not fire: the
// resettable deadline only re-arms when the body stream is polled again,
// and a dead feed stops the polling, so the very timer meant to detect the
// silence never gets re-registered after the last frame. The watchdog here
// lives in its own spawned task — driven directly by tokio's timer, not by
// hyper polling the body — and acts at the *connection* level: when a
// bookmark-negotiated watch produces no frame for
// [`WATCH_IDLE_SILENCE_LIMIT`], it flips the per-connection kill switch
// and the listener's connection task drops the socket, so the client
// reconnects (relisting when its RV has gone stale) regardless of what the
// body stream itself is doing. Must stay comfortably above the bookmark
// interval so a healthy idle watch is not churned; on a cluster where the
// datastore revision stalls for that long, a quiet-but-healthy bookmark
// watch may be ended once and relist harmlessly, which is the intended
// price of bounding staleness.
//
// The watchdog stops when the watch body it guards ends or is dropped (the
// body fires the stop signal): a watch that finished *normally* must never
// kill its connection later — an h2 connection can carry several watches,
// and a dead bookmark watch ending the connection is the recovery, but a
// finished watch doing the same would churn healthy siblings.
//
// ## Root-cause instrumentation
//
// The watchdog is also the diagnostic window into *why* a feed went dead —
// the precipitating per-connection mechanism was never pinned from the
// e2e journals, and bounding staleness without knowing the cause would
// leave the underlying failure in place. Every watchdog iteration samples
// the body's poll counters (see [`WatchIdleTracker`]), so when it fires it
// can report whether the stall happened *above* this body (hyper stopped
// polling it — the body's `poll_frame` was never called, so `polls` did
// not advance) or *below* it (the body kept being polled but produced
// nothing — the broadcast receiver was never woken, so `polls` advanced
// while `frames` did not). That distinction is the fork in the road for
// the real root cause: a frozen poll counter points at hyper's connection
// task, an advancing one at the watch's event subscription. The periodic
// `watch_idle_heartbeat` debug line (every iteration while healthy, under
// the same `nk_watch_trace` target the e2e environment already enables)
// gives the time series of that state at 75s resolution, and the live
// half of the body logs the one other silent way a watch can end
// (`Lagged`/`Closed` on the broadcast receiver).
//
// The connection close is the *recovery*, and it can also be the mask: a
// stall that is bounded and healed in ~75s never surfaces anywhere except
// the `watch_idle_killed` warn. When tracing a real failure that the bound
// keeps hiding, set [`WATCH_IDLE_KILL_DISABLED_ENV`] to `1`/`true` — the
// e2e workflow does this by default. The watchdog then runs in
// *observation mode*: identical sampling and heartbeats, plus a
// `watch_idle_stall_observed` warn at every 75s stall boundary, but it
// never flips the kill switch, so the feed failure plays out for real (the
// client stalls until its own timeout, and e2e failures reproduce with
// full diagnostics in the journals) instead of being healed invisibly.
// Unset — the default for deployments — the kill stays enabled.

// No `use std::sync::Arc` here: this file is `include!`d into
// `server::listener`, which already imports `Arc` — a second import of
// the same name would be a duplicate-definition error (E0252).
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::watch;
use tokio::time::Instant;

/// How long a bookmark-negotiated watch may produce nothing before the
/// connection is closed for the client to reconnect and relist.
pub const WATCH_IDLE_SILENCE_LIMIT: Duration = Duration::from_secs(75);

/// Environment variable that disables the idle watchdog's connection kill,
/// leaving only its instrumentation. Set to `1` or `true` (the e2e
/// workflow does this by default) to reproduce a real watch-feed stall
/// instead of having the watchdog close the connection and heal it in
/// ~75s: the `watch_idle_stall_observed` warns then mark every 75s of
/// silence with the poll/frame diagnostics while the underlying failure
/// plays out for the client to hit. Unset — the default — the watchdog
/// kills as usual.
pub const WATCH_IDLE_KILL_DISABLED_ENV: &str = "NODEAPISERVER_WATCH_IDLE_KILL_DISABLED";

/// Outcome of a watch body's most recent poll, recorded so the watchdog's
/// kill-time diagnostic can say whether the body was being driven at all.
pub const POLL_OUTCOME_PENDING: u8 = 1;
pub const POLL_OUTCOME_FRAME: u8 = 2;
pub const POLL_OUTCOME_ENDED: u8 = 3;

fn poll_outcome_label(outcome: u8) -> &'static str {
    match outcome {
        POLL_OUTCOME_PENDING => "pending",
        POLL_OUTCOME_FRAME => "frame",
        POLL_OUTCOME_ENDED => "ended",
        _ => "never",
    }
}

/// Per-connection kill switch handed to every request on that connection via
/// hyper's request extensions. A watch watchdog calls [`send`] once the
/// idle bound is exceeded; the connection task in `server::listener::run`
/// selects on the receiving end and drops the connection, ending whatever
/// body (watch stream included) is still being served on it.
///
/// [`send`]: watch::Sender::send
#[derive(Clone)]
pub struct WatchConnectionKill(pub watch::Sender<bool>);

/// Tracks what a watch response body is doing, so the watchdog can tell
/// "healthy but quiet" (the cache's bookmark heartbeat keeps frames
/// flowing) from "dead feed" (nothing for the whole limit) — and, when it
/// fires, *where* the feed died. The body bumps these on every poll and
/// every produced frame (synchronous, lock-free stores, so a busy stream
/// is never mistaken for a dead one); the watchdog compares the counters
/// across a sleep of [`WATCH_IDLE_SILENCE_LIMIT`]:
///
/// - `poll_count` advanced but `frame_count`/`last_frame_at` did not: the
///   body was being polled yet nothing came out — the broadcast receiver
///   was never woken (a subscription-level stall).
/// - `poll_count` did not advance either: hyper stopped polling this body
///   entirely (a connection-task-level stall) — the stall is above the
///   stream machinery this crate owns.
///
/// A bare wall-clock millisecond timestamp is enough for the last-frame
/// age — the watchdog only ever asks "did anything arrive since my last
/// check?"
#[derive(Clone)]
pub struct WatchIdleTracker {
    last_frame_at: Arc<AtomicU64>,
    poll_count: Arc<AtomicU64>,
    frame_count: Arc<AtomicU64>,
    last_poll_outcome: Arc<AtomicU8>,
}

impl WatchIdleTracker {
    pub fn new() -> Self {
        Self {
            last_frame_at: Arc::new(AtomicU64::new(0)),
            poll_count: Arc::new(AtomicU64::new(0)),
            frame_count: Arc::new(AtomicU64::new(0)),
            last_poll_outcome: Arc::new(AtomicU8::new(0)),
        }
    }

    /// The body was polled. Called at the top of every `poll_frame`.
    pub fn note_poll(&self) {
        self.poll_count.fetch_add(1, Ordering::Relaxed);
    }

    /// The body produced a frame. Called when `poll_frame` yields one;
    /// also records the poll outcome so the watchdog's diagnostic knows
    /// what the last poll returned.
    pub fn note_frame(&self) {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_frame_at.store(millis, Ordering::Relaxed);
        self.frame_count.fetch_add(1, Ordering::Relaxed);
        self.last_poll_outcome.store(POLL_OUTCOME_FRAME, Ordering::Relaxed);
    }

    /// Records what the body's most recent poll returned, without a frame
    /// (`pending`, or the body ended/errored). See [`POLL_OUTCOME_PENDING`]
    /// and friends.
    pub fn note_poll_outcome(&self, outcome: u8) {
        self.last_poll_outcome.store(outcome, Ordering::Relaxed);
    }

    /// The timestamp of the last frame, or `0` if the body has produced
    /// none yet. Crate-internal: the watchdog loop and the body-side tests
    /// both read it; nothing outside this crate should.
    pub(crate) fn last_frame_at(&self) -> u64 {
        self.last_frame_at.load(Ordering::Relaxed)
    }

    pub(crate) fn poll_count(&self) -> u64 {
        self.poll_count.load(Ordering::Relaxed)
    }

    pub(crate) fn frame_count(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }

    pub(crate) fn last_poll_outcome(&self) -> u8 {
        self.last_poll_outcome.load(Ordering::Relaxed)
    }
}

impl Default for WatchIdleTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-watch watchdog handle. `spawn` starts the watchdog task for one
/// bookmark-negotiated watch and returns the bundle the response body needs:
/// the tracker it bumps on every poll/frame, and the stop signal it fires
/// when the body ends or is dropped (which exits the watchdog, so a
/// finished watch can never kill a still-live connection). The resource/
/// client identity is handed to the watchdog task for its diagnostics and
/// not retained here. Whether the watchdog may actually close the
/// connection is read once per watch from [`WATCH_IDLE_KILL_DISABLED_ENV`]
/// (see [`kill_enabled_from_env`]): the e2e default of `1`/`true` runs it
/// in observation-only mode so a real feed stall surfaces with the
/// instrumentation, while unset keeps the kill — the deployment default.
pub struct WatchIdleGuard {
    tracker: WatchIdleTracker,
    stop_tx: watch::Sender<bool>,
}

impl WatchIdleGuard {
    /// Starts the watchdog for one watch, bound to this connection's kill
    /// switch. `deadline` is the watch's own `timeoutSeconds`: instead of
    /// ending the stream in-body (whose final EOF is not reliably delivered
    /// to a client — see `watch_stream`'s own note), the watchdog fires the
    /// connection kill switch at the deadline, which the client observes as
    /// a connection close and recovers from with the same relist. Task count
    /// stays bounded by the number of active bookmark watches; each task is
    /// a sleep-and-compare loop that exits within one limit of its body
    /// ending.
    pub fn spawn(
        kill: WatchConnectionKill,
        resource: String,
        client: String,
        deadline: Option<Duration>,
    ) -> Self {
        let tracker = WatchIdleTracker::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let kill_enabled = kill_enabled_from_env();
        spawn_idle_watchdog(
            kill,
            tracker.clone(),
            stop_rx,
            resource,
            client,
            kill_enabled,
            deadline,
        );
        Self { tracker, stop_tx }
    }

    pub fn tracker(&self) -> WatchIdleTracker {
        self.tracker.clone()
    }

    pub fn stop_tx(&self) -> watch::Sender<bool> {
        self.stop_tx.clone()
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Whether the watchdog may close the connection, read once per watch from
/// [`WATCH_IDLE_KILL_DISABLED_ENV`]. The e2e workflow sets it to `true` so
/// a real feed stall surfaces with the instrumentation instead of being
/// healed by the kill; unset (or any value other than `1`/`true`) keeps
/// the kill enabled — the default for deployments.
fn kill_enabled_from_env() -> bool {
    !matches!(
        std::env::var(WATCH_IDLE_KILL_DISABLED_ENV).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// The watchdog loop behind [`spawn_idle_watchdog`], factored out with an
/// explicit `limit` so a unit test can drive a short deadline instead of
/// waiting the real 75 seconds. Spawns a fresh `limit` sleep on every
/// iteration and re-checks the tracker after it: if no frame arrived since
/// the iteration started, the feed is dead. With `kill_enabled` (the
/// default) the connection is killed for the client to relist; without it
/// (observation mode, [`WATCH_IDLE_KILL_DISABLED_ENV`] set) the loop logs
/// the same diagnostics as a `watch_idle_stall_observed` warn and keeps
/// sampling, so the underlying failure plays out while the journals
/// capture it. The sleep is a real tokio timer on this task's own waker,
/// so it fires even when hyper has parked the response body and nothing
/// is polling it. The `stop` receiver ends the loop the moment the body
/// it guards is gone. Each iteration also samples the poll counters (see
/// the module doc's "Root-cause instrumentation") and logs the healthy
/// state as a debug heartbeat or the dead state as a warn.
pub async fn watch_idle_loop(
    kill: &watch::Sender<bool>,
    tracker: &WatchIdleTracker,
    limit: Duration,
    mut stop: watch::Receiver<bool>,
    resource: String,
    client: String,
    kill_enabled: bool,
    deadline: Option<Duration>,
) {
    let mut last_polls = tracker.poll_count();
    let mut last_frames = tracker.frame_count();
    // The watch's `timeoutSeconds`, as an instant from now: at the deadline
    // the watchdog closes the connection (same client relist as the stream
    // ending, delivered reliably) instead of relying on the in-body
    // `take_until` whose final EOF is not guaranteed to reach the client.
    let mut deadline_at = deadline.map(|d| Instant::now() + d);
    loop {
        let last_frame = tracker.last_frame_at();
        // Wake at whichever comes first: the next silence check or the
        // timeout deadline (which needs sub-limit precision so a healthy
        // watch still relists at its own `timeoutSeconds`, not a full
        // silence-limit late).
        let silence_at = Instant::now() + limit;
        let next_wake = match deadline_at {
            Some(deadline_at) => deadline_at.min(silence_at),
            None => silence_at,
        };
        tokio::select! {
            _ = tokio::time::sleep_until(next_wake) => {
                if let Some(deadline) = deadline_at {
                    if Instant::now() >= deadline {
                        // The watch reached its `timeoutSeconds`. Consume
                        // the deadline so observation mode falls back to the
                        // ordinary silence sampling; with the kill enabled
                        // the connection is closed for the client to relist
                        // (the recovery a clean EOF was meant to trigger).
                        deadline_at = None;
                        let polls = tracker.poll_count();
                        let frames = tracker.frame_count();
                        let polls_in_window = polls.saturating_sub(last_polls);
                        let frames_in_window = frames.saturating_sub(last_frames);
                        if kill_enabled {
                            tracing::info!(
                                target: "nk_watch_trace",
                                boundary = "watch_idle_timeout_killed",
                                resource = %resource,
                                client = %client,
                                polls_in_window,
                                frames_in_window,
                                last_poll = poll_outcome_label(tracker.last_poll_outcome()),
                                total_frames = frames,
                                "watch idle: bookmark watch reached its timeoutSeconds deadline; closing the connection so the client relists (the in-body stream end is not delivered reliably enough to depend on)"
                            );
                            let _ = kill.send(true);
                            return;
                        }
                        tracing::debug!(
                            target: "nk_watch_trace",
                            boundary = "watch_idle_timeout_observed",
                            resource = %resource,
                            client = %client,
                            polls_in_window,
                            frames_in_window,
                            last_poll = poll_outcome_label(tracker.last_poll_outcome()),
                            total_frames = frames,
                            "watch idle: bookmark watch reached its timeoutSeconds deadline (observation mode: the connection kill is disabled, so the stream stays open for the journals)"
                        );
                        last_polls = polls;
                        last_frames = frames;
                        continue;
                    }
                }
                let polls = tracker.poll_count();
                let frames = tracker.frame_count();
                let polls_in_window = polls.saturating_sub(last_polls);
                let frames_in_window = frames.saturating_sub(last_frames);
                if tracker.last_frame_at() == last_frame {
                    let silence_secs = if last_frame == 0 {
                        limit.as_secs()
                    } else {
                        now_millis().saturating_sub(last_frame) / 1000
                    };
                    if kill_enabled {
                        tracing::warn!(
                            target: "nk_watch_trace",
                            boundary = "watch_idle_killed",
                            resource = %resource,
                            client = %client,
                            silence_secs,
                            polls_in_window,
                            frames_in_window,
                            polled_by_hyper = polls_in_window > 0,
                            last_poll = poll_outcome_label(tracker.last_poll_outcome()),
                            total_frames = frames,
                            "watch idle: bookmark watch produced no frame for the silence limit; closing the connection so the client relists. polled_by_hyper=false means hyper stopped polling this body (connection-task stall); true means the body was polled but the event subscription yielded nothing"
                        );
                        // `false` -> `true` wakes the connection task's
                        // `changed()` select arm; a subsequent send (or
                        // the sender being dropped) is irrelevant because
                        // the connection is already gone.
                        let _ = kill.send(true);
                        return;
                    }
                    tracing::warn!(
                        target: "nk_watch_trace",
                        boundary = "watch_idle_stall_observed",
                        resource = %resource,
                        client = %client,
                        silence_secs,
                        polls_in_window,
                        frames_in_window,
                        polled_by_hyper = polls_in_window > 0,
                        last_poll = poll_outcome_label(tracker.last_poll_outcome()),
                        total_frames = frames,
                        "watch idle: bookmark watch produced no frame for the silence limit (observation mode: the connection kill is disabled so the underlying failure can be traced). polled_by_hyper=false means hyper stopped polling this body (connection-task stall); true means the body was polled but the event subscription yielded nothing"
                    );
                    // Observation mode: do not close the connection — the
                    // point is to let the feed failure play out while the
                    // journals capture it. Keep sampling so the next
                    // window reports a fresh baseline.
                } else {
                    tracing::debug!(
                        target: "nk_watch_trace",
                        boundary = "watch_idle_heartbeat",
                        resource = %resource,
                        client = %client,
                        polls_in_window,
                        frames_in_window,
                        last_poll = poll_outcome_label(tracker.last_poll_outcome()),
                        total_frames = frames,
                        "watch idle watchdog healthy"
                    );
                }
                last_polls = polls;
                last_frames = frames;
            }
            _ = stop.changed() => {
                // The watch body ended (or was dropped): nothing left to
                // guard, and killing the connection now would only churn
                // whatever siblings still share it.
                return;
            }
        }
    }
}

/// Starts the per-watch watchdog described in this module's doc comment.
/// One task per bookmark-negotiated watch, alive for at most
/// [`WATCH_IDLE_SILENCE_LIMIT`] past its last frame, and exited as soon as
/// its body ends — so the task count stays bounded by the number of active
/// bookmark watches. `deadline` is the watch's `timeoutSeconds`, owned by
/// this watchdog rather than an in-body stream end (see `watch_idle_loop`).
pub fn spawn_idle_watchdog(
    kill: WatchConnectionKill,
    tracker: WatchIdleTracker,
    stop: watch::Receiver<bool>,
    resource: String,
    client: String,
    kill_enabled: bool,
    deadline: Option<Duration>,
) {
    tokio::spawn(async move {
        watch_idle_loop(&kill.0, &tracker, WATCH_IDLE_SILENCE_LIMIT, stop, resource, client, kill_enabled, deadline).await;
    });
}

// This file is `include!`d into `server::listener`, whose own test module
// (`listener_tests.rs`) is also included there — a second `mod tests`
// would be a duplicate-definition error (E0428), hence the distinct name.
#[cfg(test)]
mod watch_idle_tests {
    use super::*;

    /// A stop channel whose sender stays alive for the caller's scope —
    /// dropping the sender would make `stop.changed()` return `Err`
    /// immediately and exit the watchdog before its first sleep.
    fn never_stop() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    fn loop_args(resource: &str) -> (String, String) {
        (resource.to_string(), "test-client".to_string())
    }

    /// Spawns the watchdog loop with owned clones of the shared state, the
    /// way `spawn_idle_watchdog` does — `tokio::spawn` needs a `'static`
    /// future, so borrowing test locals directly would not compile.
    fn spawn_loop(
        kill: &watch::Sender<bool>,
        tracker: &WatchIdleTracker,
        limit: Duration,
        stop: watch::Receiver<bool>,
        resource: String,
        client: String,
        kill_enabled: bool,
        deadline: Option<Duration>,
    ) -> tokio::task::JoinHandle<()> {
        let kill = WatchConnectionKill(kill.clone());
        let tracker = tracker.clone();
        tokio::spawn(async move {
            watch_idle_loop(&kill.0, &tracker, limit, stop, resource, client, kill_enabled, deadline).await;
        })
    }

    #[tokio::test]
    async fn watchdog_ends_a_watch_that_never_yields() {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let started = std::time::Instant::now();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            true,
            None,
        );
        let killed = tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
            .await
            .expect("a silent bookmark watch must be closed by the idle watchdog");
        assert!(killed.is_ok());
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "the watchdog must wait out its limit, not kill immediately"
        );
    }

    #[tokio::test]
    async fn watchdog_does_not_kill_while_frames_keep_flowing() {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            true,
            None,
        );
        // Frames every 40ms — each far below the 100ms limit — for 320ms
        // total. A fixed-lifetime bound would have fired by now.
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tracker.note_poll();
            tracker.note_frame();
        }
        assert!(
            !*kill_rx.borrow(),
            "the idle watchdog must not fire while frames keep flowing"
        );
        // Now go silent: the next check must close the connection within
        // one limit of the last frame.
        let started = std::time::Instant::now();
        let killed = tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
            .await
            .expect("silence after the last frame must close the connection");
        assert!(killed.is_ok());
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "the deadline must be measured from the last frame, not run from watchdog start"
        );
    }

    #[tokio::test]
    async fn watchdog_survives_a_late_frame_and_resets_from_it() {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let started = std::time::Instant::now();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            true,
            None,
        );
        // One frame just past half the limit: the watchdog must not close
        // at the original deadline — it must wait a fresh full limit from
        // the frame.
        tokio::time::sleep(Duration::from_millis(60)).await;
        tracker.note_poll();
        tracker.note_frame();
        let killed = tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
            .await
            .expect("silence after the late frame must eventually close the connection");
        assert!(killed.is_ok());
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the deadline must reset from the last frame, not run from watchdog start"
        );
    }

    #[tokio::test]
    async fn watchdog_exits_when_the_watch_body_ends() {
        let (kill_tx, kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (resource, client) = loop_args("v1/namespaces");
        let task = spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            true,
            None,
        );
        // The body ends (or is dropped) long before the idle limit; the
        // watchdog must exit without ever killing the connection.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = stop_tx.send(true);
        task.await.unwrap();
        assert!(
            !*kill_rx.borrow(),
            "a finished watch must not kill its connection later"
        );
    }

    #[tokio::test]
    async fn observation_mode_never_kills_but_keeps_sampling_stalls() {
        let (kill_tx, kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        let task = spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            false,
            None,
        );
        // Several consecutive silence windows: every one is an observed
        // stall (`watch_idle_stall_observed`), and none may kill.
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(
            !*kill_rx.borrow(),
            "observation mode must never fire the connection kill switch"
        );
        assert!(
            !task.is_finished(),
            "observation mode must keep sampling after observed stalls"
        );
        // A frame resumes the healthy path; still no kill.
        tracker.note_poll();
        tracker.note_frame();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !*kill_rx.borrow(),
            "a resumed feed must not trip the kill even after observed stalls"
        );
        assert!(
            !task.is_finished(),
            "the watchdog must keep running after a stall is observed and the feed resumes"
        );
    }

    #[tokio::test]
    async fn watchdog_diagnostic_distinguishes_polled_from_unpolled_silence() {
        let tracker = WatchIdleTracker::new();
        // The body keeps being polled (hyper alive) but yields nothing: the
        // counters must reflect "polled, no frames", the signature of a
        // subscription-level stall rather than a connection-task one.
        tracker.note_poll();
        tracker.note_poll();
        tracker.note_poll_outcome(POLL_OUTCOME_PENDING);
        assert_eq!(tracker.poll_count(), 2);
        assert_eq!(tracker.frame_count(), 0);
        assert_eq!(tracker.last_poll_outcome(), POLL_OUTCOME_PENDING);
        assert_eq!(poll_outcome_label(tracker.last_poll_outcome()), "pending");
    }

    #[tokio::test]
    async fn watchdog_closes_the_connection_at_the_timeout_deadline_even_while_frames_flow() {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let started = std::time::Instant::now();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        // Limit is 200ms (the silence bound would not fire for 200ms of
        // quiet), but the watch's own `timeoutSeconds` deadline is 250ms:
        // a healthy stream that keeps producing frames below the silence
        // limit must still be ended at its deadline — that is the client
        // relist cadence the in-body `take_until` used to provide (when it
        // happened to be delivered).
        spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(200),
            stop_rx,
            resource,
            client,
            true,
            Some(Duration::from_millis(250)),
        );
        // Frames every 40ms, well under the 200ms silence limit, until past
        // the 250ms deadline.
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tracker.note_poll();
            tracker.note_frame();
            if started.elapsed() >= Duration::from_millis(220) {
                break;
            }
        }
        let killed = tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
            .await
            .expect("the watchdog must fire the kill at the watch's timeoutSeconds deadline");
        assert!(killed.is_ok());
        assert!(
            started.elapsed() >= Duration::from_millis(240),
            "the deadline kill must not fire before the configured timeout"
        );
    }

    #[tokio::test]
    async fn watchdog_deadline_never_fires_before_frames_stop_when_deadline_is_later_than_silence() {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        // Deadline well past the silence limit: with frames flowing, the
        // silence check must not fire, and the deadline must not fire early.
        spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(100),
            stop_rx,
            resource,
            client,
            true,
            Some(Duration::from_millis(500)),
        );
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tracker.note_poll();
            tracker.note_frame();
        }
        assert!(
            !*kill_rx.borrow(),
            "neither the silence bound nor the deadline may fire while frames keep flowing"
        );
        // Once the feed goes quiet the silence bound closes the connection
        // well before the (irrelevant now) 500ms deadline.
        let killed = tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
            .await
            .expect("silence must close the connection before the later deadline");
        assert!(killed.is_ok());
    }

    #[tokio::test]
    async fn observation_mode_logs_the_deadline_but_keeps_the_stream_open() {
        let (kill_tx, kill_rx) = watch::channel(false);
        let tracker = WatchIdleTracker::new();
        let (resource, client) = loop_args("v1/namespaces");
        let (_stop_tx, stop_rx) = never_stop();
        let task = spawn_loop(
            &kill_tx,
            &tracker,
            Duration::from_millis(200),
            stop_rx,
            resource,
            client,
            false,
            Some(Duration::from_millis(120)),
        );
        // Deadline passes while frames keep flowing below the silence limit:
        // observation mode must log (`watch_idle_timeout_observed`) but
        // never close the connection.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tracker.note_poll();
            tracker.note_frame();
        }
        assert!(
            !*kill_rx.borrow(),
            "observation mode must not close the connection at the deadline"
        );
        assert!(
            !task.is_finished(),
            "observation mode must keep sampling after the deadline passes"
        );
        // Still alive after the deadline has been consumed and silence
        // resumes being sampled.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!task.is_finished());
        assert!(!*kill_rx.borrow());
    }
}