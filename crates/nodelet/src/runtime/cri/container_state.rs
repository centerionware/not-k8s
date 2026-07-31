use super::*;

/// What ensure_container() should do about an already-existing container
/// with the target name, given its CRI state and the pod's restartPolicy.
/// Pulled out as a pure decision (see restart_decision()) specifically so
/// the restart-on-exit fix (crates the whole coredns pile-up traced back
/// to) has a unit-testable matrix instead of only being verifiable by
/// hand against a real cluster.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RestartDecision {
    /// Already running — leave it alone.
    AlreadyRunning,
    /// Not running, but restartPolicy: Never means it's done for good —
    /// leave it alone (Job-style one-shot semantics).
    LeaveTerminated,
    /// Not running and this pod is allowed to restart — remove the stale
    /// container and create a fresh one.
    NeedsRestart,
}


/// What ensure_pod() should do about a sandbox lookup result, given its CRI
/// state. Pulled out as a pure decision for the same reason as
/// restart_decision() above: this exact bug (reusing a dead sandbox forever
/// after a reboot) was only found by hand, against a real device, and
/// deserves a matrix that doesn't require one to catch again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SandboxDecision {
    /// A ready sandbox exists — use it as-is.
    Reuse,
    /// A sandbox exists but isn't ready (its task/pause process is gone,
    /// e.g. after a reboot) — tear it down and create a fresh one.
    RecreateStale,
    /// No sandbox at all — create one.
    CreateFresh,
}


pub(crate) fn sandbox_reuse_decision(found: Option<i32>, ready_state: i32) -> SandboxDecision {
    match found {
        Some(s) if s == ready_state => SandboxDecision::Reuse,
        Some(_) => SandboxDecision::RecreateStale,
        None => SandboxDecision::CreateFresh,
    }
}


/// What `ensure_init_containers()` should do about one init container, given
/// its CRI state (if it exists at all) and — if exited — its exit code.
/// Pulled out as a pure decision for the same reason `restart_decision()`
/// and `compute_phase()` are: this is the exact logic that decides whether
/// init containers gate app containers correctly, and deserves a matrix
/// that doesn't require a live CRI socket to verify.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InitContainerDecision {
    /// Doesn't exist yet — create and start it.
    Create,
    /// Running — wait for it.
    StillRunning,
    /// Exited zero — this one's done; check the next init container.
    Done,
    /// Exited nonzero and this pod is allowed to restart — remove it so a
    /// fresh one gets created.
    Retry,
    /// Exited nonzero under `restartPolicy: Never` — terminal.
    Failed,
    /// Neither running nor exited (e.g. still being created) — wait.
    Waiting,
}


pub(crate) fn init_container_decision(
    existing_state: Option<i32>,
    running_state: i32,
    exited_state: i32,
    exit_code: i32,
    restart_policy: &str,
) -> InitContainerDecision {
    match existing_state {
        None => InitContainerDecision::Create,
        Some(s) if s == running_state => InitContainerDecision::StillRunning,
        Some(s) if s == exited_state => {
            if exit_code == 0 {
                InitContainerDecision::Done
            } else if restart_policy == "Never" {
                InitContainerDecision::Failed
            } else {
                InitContainerDecision::Retry
            }
        }
        Some(_) => InitContainerDecision::Waiting,
    }
}


/// Where `ensure_init_containers()` left off.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InitProgress {
    /// The next not-yet-done init container was just created, or is still
    /// running, or is done and something after it isn't — either way, the
    /// app containers must not start yet.
    Waiting,
    /// An init container exited nonzero under `restartPolicy: Never` —
    /// terminal, matches kubelet reporting the whole Pod `Failed`.
    Failed(String),
    /// Every init container has exited zero, in order — start the app containers.
    AllComplete,
}


/// What `ensure_init_containers()` should do about one native sidecar
/// container (`initContainers[].restartPolicy: "Always"`, round 36),
/// given its CRI state if it exists at all. Unlike a regular init
/// container (`InitContainerDecision`), a sidecar never blocks later
/// containers on its own *exit* — only on having been created at all —
/// and restarts on exit like a normal container, indefinitely, for the
/// pod's whole lifetime. Pulled out as a pure decision for the same
/// reason `init_container_decision()`/`restart_decision()` are.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SidecarInitDecision {
    /// Doesn't exist yet — create it, and block later containers until
    /// it's at least been created (matching upstream's "wait for Started"
    /// gate, approximated here as "creation issued").
    Create,
    /// Exited — restart it, but don't block later containers on this.
    NeedsRestart,
    /// Running (or some other transient CRI state) — already started;
    /// don't block later containers.
    Started,
}


pub(crate) fn sidecar_init_decision(existing_state: Option<i32>, running_state: i32, exited_state: i32) -> SidecarInitDecision {
    match existing_state {
        None => SidecarInitDecision::Create,
        Some(s) if s == exited_state => SidecarInitDecision::NeedsRestart,
        Some(s) if s == running_state => SidecarInitDecision::Started,
        Some(_) => SidecarInitDecision::Started, // some other transient CRI state — don't block on it either
    }
}


pub(crate) fn restart_decision(existing_state: Option<i32>, running_state: i32, restart_policy: &str) -> RestartDecision {
    match existing_state {
        None => RestartDecision::NeedsRestart, // no existing container at all — same code path as a genuine restart
        Some(s) if s == running_state => RestartDecision::AlreadyRunning,
        Some(_) if restart_policy == "Never" => RestartDecision::LeaveTerminated,
        Some(_) => RestartDecision::NeedsRestart,
    }
}


/// Pod-level phase from container CRI states + restartPolicy. See the
/// long comment on build_status()'s call site for why restartPolicy has to
/// factor in here — reporting Succeeded for a restartPolicy: Always pod
/// whose container merely exited is the bug that drove unbounded coredns
/// pod creation (Kubernetes' ReplicaSet controller treats Succeeded/Failed
/// pods as permanently inactive and replaces them).
/// `any_failed` only matters (and is only computed by the caller) when
/// `restart_policy == "Never"` and every container has exited — otherwise a
/// nonzero exit just means "will be restarted", not "this pod failed".
/// Before this, `restartPolicy: Never` always reported `Succeeded` even when
/// a container exited nonzero — a Job-style pod that actually failed looked
/// like it succeeded.
pub(crate) fn compute_phase(any_running: bool, all_exited: bool, any_failed: bool, restart_policy: &str) -> Phase {
    if any_running {
        Phase::Running
    } else if all_exited && restart_policy == "Never" {
        if any_failed {
            Phase::Failed
        } else {
            Phase::Succeeded
        }
    } else {
        Phase::Pending
    }
}


/// Pure restart-count-table logic, pulled out of `CriRuntime`'s methods so
/// it's unit-testable without a real CRI socket/kube client (a `CriRuntime`
/// can't be constructed without both).
pub(crate) fn restart_count_key(sandbox_id: &str, container_name: &str) -> String {
    format!("{sandbox_id}/{container_name}")
}


pub(crate) fn restart_count_from(counts: &HashMap<String, u32>, sandbox_id: &str, container_name: &str) -> u32 {
    counts.get(&restart_count_key(sandbox_id, container_name)).copied().unwrap_or(0)
}


pub(crate) fn bump_restart_count_in(counts: &mut HashMap<String, u32>, sandbox_id: &str, container_name: &str) -> u32 {
    let entry = counts.entry(restart_count_key(sandbox_id, container_name)).or_insert(0);
    *entry += 1;
    *entry
}


pub(crate) fn clear_restart_counts_in(counts: &mut HashMap<String, u32>, sandbox_id: &str) {
    let prefix = format!("{sandbox_id}/");
    counts.retain(|k, _| !k.starts_with(&prefix));
}


/// Crash-loop backoff (round 73; found in round 72's re-audit). Base
/// delay and cap match real kubelet's own `flowcontrol.Backoff` defaults
/// exactly (10s base, doubling, capped at 5 minutes); the reset-to-base
/// threshold (10 minutes of no restart attempt) mirrors upstream's own
/// "twice the max backoff" reset rule.
pub(crate) const CRASH_LOOP_BACKOFF_BASE_SECS: u64 = 10;
pub(crate) const CRASH_LOOP_BACKOFF_MAX_SECS: u64 = 300;
pub(crate) const CRASH_LOOP_BACKOFF_RESET_SECS: u64 = 2 * CRASH_LOOP_BACKOFF_MAX_SECS;


/// The backoff delay (in seconds) to require before the *next* restart
/// attempt, given the delay required before the restart attempt that
/// just happened (`prev_backoff_secs`, `None` if this is the very first
/// restart ever recorded for this container) and how long ago that prior
/// attempt actually was (`elapsed_since_prev_secs`). Doubles on a
/// still-failing container, capped at the max; resets back to the base
/// delay once the container has gone long enough without needing another
/// restart attempt to count as having stabilized.
pub(crate) fn crash_loop_backoff_secs(prev_backoff_secs: Option<u64>, elapsed_since_prev_secs: Option<u64>) -> u64 {
    let should_reset = match (prev_backoff_secs, elapsed_since_prev_secs) {
        (Some(_), Some(elapsed)) => elapsed >= CRASH_LOOP_BACKOFF_RESET_SECS,
        _ => true,
    };
    if should_reset {
        CRASH_LOOP_BACKOFF_BASE_SECS
    } else {
        (prev_backoff_secs.unwrap() * 2).min(CRASH_LOOP_BACKOFF_MAX_SECS)
    }
}


/// Whether enough time has passed since the last restart attempt
/// (`last_restart_unix`, `None` if this container has never been
/// restarted before) to allow another one now, given the backoff delay
/// that was required after that last attempt.
pub(crate) fn crash_loop_backoff_ready(last_restart_unix: Option<u64>, required_backoff_secs: u64, now_unix: u64) -> bool {
    match last_restart_unix {
        None => true,
        Some(t) => now_unix.saturating_sub(t) >= required_backoff_secs,
    }
}


pub(crate) fn clear_restart_backoff_in(backoff: &mut HashMap<String, (u64, u64)>, sandbox_id: &str) {
    let prefix = format!("{sandbox_id}/");
    backoff.retain(|k, _| !k.starts_with(&prefix));
}


/// Real kubelet default when `terminationGracePeriodSeconds` is unset (or
/// explicitly negative, which the API otherwise allows through): 30s.
pub(crate) fn termination_grace_seconds(pod: &Pod) -> i64 {
    match pod.spec.as_ref().and_then(|s| s.termination_grace_period_seconds) {
        Some(s) if s >= 0 => s,
        _ => 30,
    }
}


impl CriRuntime {
    /// Drop every restart-count entry for a sandbox that's gone (removed or
    /// recreated-stale) — otherwise this side table grows forever across
    /// pod recreations.
    pub(crate) fn clear_restart_counts(&self, sandbox_id: &str) {
        clear_restart_counts_in(&mut self.restart_counts.lock().unwrap(), sandbox_id);
    }

    pub(crate) fn restart_count(&self, sandbox_id: &str, container_name: &str) -> u32 {
        restart_count_from(&self.restart_counts.lock().unwrap(), sandbox_id, container_name)
    }

    /// Bump and return the new restart count for a container that's about
    /// to be recreated after actually having existed before (not the very
    /// first creation — see the `NeedsRestart` branches' `existing_ctr` check).
    pub(crate) fn bump_restart_count(&self, sandbox_id: &str, container_name: &str) -> u32 {
        bump_restart_count_in(&mut self.restart_counts.lock().unwrap(), sandbox_id, container_name)
    }

    /// Drop crash-loop backoff state for a sandbox that's gone, same
    /// reason/lifecycle as `clear_restart_counts()`.
    pub(crate) fn clear_restart_backoff(&self, sandbox_id: &str) {
        clear_restart_backoff_in(&mut self.restart_backoff.lock().unwrap(), sandbox_id);
    }

    /// Whether a container due for a restart is allowed to restart right
    /// now, per crash-loop backoff. Read-only — does NOT record this
    /// attempt; call `record_restart_backoff()` once the restart actually
    /// happens (a caller that decides to skip the restart this time
    /// leaves the recorded state untouched, so the same backoff window
    /// keeps counting down rather than being pushed back out).
    pub(crate) fn restart_backoff_ready(&self, sandbox_id: &str, container_name: &str) -> bool {
        let key = restart_count_key(sandbox_id, container_name);
        let now = now_unix_secs();
        let table = self.restart_backoff.lock().unwrap();
        match table.get(&key) {
            Some((last_restart_unix, required_backoff_secs)) => crash_loop_backoff_ready(Some(*last_restart_unix), *required_backoff_secs, now),
            None => true,
        }
    }

    /// Record that a restart attempt just happened, computing the backoff
    /// delay required before the *next* one is allowed.
    pub(crate) fn record_restart_backoff(&self, sandbox_id: &str, container_name: &str) {
        let key = restart_count_key(sandbox_id, container_name);
        let now = now_unix_secs();
        let mut table = self.restart_backoff.lock().unwrap();
        let prev = table.get(&key).copied();
        let elapsed = prev.map(|(last, _)| now.saturating_sub(last));
        let new_backoff = crash_loop_backoff_secs(prev.map(|(_, b)| b), elapsed);
        table.insert(key, (now, new_backoff));
    }
}


fn now_unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
