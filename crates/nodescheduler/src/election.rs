//! Leader election over a `coordination.k8s.io/v1` Lease.
//!
//! # Why this is new code rather than a copy of something in this repo
//!
//! `nodelet` already writes a Lease, and it is the wrong shape to borrow. That
//! one is a node-liveness heartbeat written with server-side apply and
//! `.force()` — i.e. "this record is mine, overwrite whatever is there". That
//! is exactly correct for a heartbeat, where a node is the sole author of its
//! own liveness, and exactly wrong here, where the entire purpose is to find
//! out whether someone else got there first.
//!
//! So this uses optimistic concurrency instead: read the Lease, decide, and
//! write it back with the `resourceVersion` we read. If another replica wrote
//! in between, the apiserver rejects the update with a conflict and we lost
//! the race — which is the answer we were asking for, not an error to retry
//! blindly.
//!
//! # What a scheduler needs it for
//!
//! A scheduler is a single writer of `Binding`. Two of them watching the same
//! unbound pods will both decide, both bind, and the loser's decision is
//! discarded by the apiserver — but only *after* both have assumed the pod
//! onto their own chosen node, so both caches are now wrong about where
//! capacity went. This project targets clusters with several control-plane
//! nodes, so that is a live concern rather than a theoretical one.
//!
//! # Losing the lease must stop the work immediately
//!
//! [`run_as_leader`] runs the caller's future *inside* a `select!` against
//! the renewal loop, so the moment renewal fails past the deadline the work
//! is dropped. A scheduler that keeps binding after losing leadership is
//! precisely the race the lease exists to prevent — and it is worse than
//! having no lease at all, because the other replica is now also scheduling
//! and neither knows.
//!
//! The timing rule is `retryPeriod < renewDeadline < leaseDuration`, enforced
//! in `config.rs`. The gap between giving up (`renewDeadline`) and another
//! replica being allowed to take over (`leaseDuration`) is the safety margin:
//! we stop before anyone else may start.

use crate::config::Config;
use anyhow::{Context, Result};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::{Span, Timestamp};
use kube::api::{Api, PostParams};
use std::time::{Duration, Instant};

/// Whether we may take a lease, given what is currently recorded in it.
///
/// Pure so the decision is testable without an apiserver — the same split the
/// rest of this crate uses.
pub fn may_acquire(
    holder: Option<&str>,
    renewed_at: Option<Timestamp>,
    lease_duration: Duration,
    now: Timestamp,
    me: &str,
) -> bool {
    // Unheld, or held by a previous incarnation of us.
    match holder {
        None | Some("") => return true,
        Some(h) if h == me => return true,
        _ => {}
    }
    // Held by someone else: only if they have stopped renewing for longer
    // than the full lease duration. Note this is deliberately generous —
    // taking over early is how two schedulers end up both believing they are
    // leader.
    match renewed_at {
        None => true,
        Some(t) => {
            let expiry = t + Span::new().seconds(lease_duration.as_secs() as i64);
            now > expiry
        }
    }
}

/// A held lease, and what it takes to keep holding it.
pub struct LeaderLease {
    api: Api<Lease>,
    name: String,
    identity: String,
    lease_duration: Duration,
    renew_deadline: Duration,
    retry_period: Duration,
}

impl LeaderLease {
    pub fn new(client: kube::Client, cfg: &Config) -> Self {
        Self {
            api: Api::namespaced(client, &cfg.lease_namespace),
            name: cfg.lease_name.clone(),
            identity: cfg.holder_identity.clone(),
            lease_duration: cfg.lease_duration,
            renew_deadline: cfg.renew_deadline,
            retry_period: cfg.retry_period,
        }
    }

    /// Try once to take the lease. `Ok(false)` means someone else holds it —
    /// an ordinary outcome, not a failure.
    async fn try_acquire(&self) -> Result<bool> {
        let now = Timestamp::now();

        match self.api.get_opt(&self.name).await.context("reading the scheduler lease")? {
            None => {
                // First scheduler in a fresh cluster. A create that loses the
                // race returns AlreadyExists, which is simply "not us".
                let lease = self.lease_object(None, now, now, 1);
                match self.api.create(&PostParams::default(), &lease).await {
                    Ok(_) => Ok(true),
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                    Err(e) => Err(e).context("creating the scheduler lease"),
                }
            }
            Some(existing) => {
                self.replace_with_retry(existing).await
            }
        }
    }

    /// Retry an optimistic lease update only after re-reading the object. A
    /// status writer can legitimately advance the Lease between our GET and
    /// replace; abandoning leadership on that first 409 makes a healthy
    /// scheduler flap out of an ordinary concurrent write. Re-evaluating the
    /// fresh holder on every attempt preserves the safety rule: if another
    /// identity now owns a live lease, return `Ok(false)` and stop scheduling.
    async fn replace_with_retry(&self, mut existing: Lease) -> Result<bool> {
        const MAX_CONFLICT_RETRIES: usize = 3;
        const CONFLICT_RETRY_DELAY: Duration = Duration::from_millis(25);

        for attempt in 0..=MAX_CONFLICT_RETRIES {
            let now = Timestamp::now();
            let spec = existing.spec.clone().unwrap_or_default();
            let holder = spec.holder_identity.clone();
            let renewed = spec.renew_time.as_ref().map(|t| t.0);
            if !may_acquire(
                holder.as_deref(),
                renewed,
                self.lease_duration,
                now,
                &self.identity,
            ) {
                return Ok(false);
            }

            // A transition is a change of holder, not a renewal by the same
            // one. A renewal keeps acquireTime so operators can distinguish
            // a long-held term from leadership flapping.
            let transitions = spec.lease_transitions.unwrap_or(0)
                + i32::from(holder.as_deref() != Some(self.identity.as_str()));
            let acquired_at = if holder.as_deref() == Some(self.identity.as_str()) {
                spec.acquire_time.map(|t| t.0).unwrap_or(now)
            } else {
                now
            };
            let lease = self.lease_object(
                existing.metadata.resource_version.clone(),
                now,
                acquired_at,
                transitions,
            );

            // Carrying the resourceVersion we read is what turns a lost race
            // into a 409 instead of a silent double-leader. A 409 is
            // recoverable only by obtaining a fresh resourceVersion and
            // checking the holder again.
            match self.api.replace(&self.name, &PostParams::default(), &lease).await {
                Ok(_) => return Ok(true),
                Err(kube::Error::Api(e)) if e.code == 409 => {
                    if attempt == MAX_CONFLICT_RETRIES {
                        return Err(anyhow::anyhow!(
                            "scheduler lease update conflicted after {} attempts",
                            attempt + 1
                        ));
                    }
                    tokio::time::sleep(CONFLICT_RETRY_DELAY).await;
                    existing = self
                        .api
                        .get_opt(&self.name)
                        .await
                        .context("re-reading the scheduler lease after a conflict")?
                        .ok_or_else(|| anyhow::anyhow!("scheduler lease disappeared during renewal"))?;
                }
                Err(e) => return Err(e).context("acquiring the scheduler lease"),
            }
        }
        unreachable!("lease conflict retry loop always returns")
    }

    fn lease_object(
        &self,
        resource_version: Option<String>,
        now: Timestamp,
        acquired_at: Timestamp,
        transitions: i32,
    ) -> Lease {
        Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                resource_version,
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(self.identity.clone()),
                lease_duration_seconds: Some(self.lease_duration.as_secs() as i32),
                acquire_time: Some(MicroTime(acquired_at)),
                renew_time: Some(MicroTime(now)),
                lease_transitions: Some(transitions),
                ..Default::default()
            }),
        }
    }

    /// Block until this instance holds the lease.
    pub async fn acquire(&self) -> Result<()> {
        let mut announced = false;
        loop {
            match self.try_acquire().await {
                Ok(true) => {
                    tracing::info!(
                        lease = %self.name,
                        identity = %self.identity,
                        "acquired leadership; this instance is now scheduling"
                    );
                    return Ok(());
                }
                Ok(false) => {
                    if !announced {
                        tracing::info!(
                            lease = %self.name,
                            "another scheduler holds the lease; standing by (this instance \
                             watches nothing and schedules nothing until it takes over)"
                        );
                        announced = true;
                    }
                }
                // A transient apiserver problem must not be mistaken for
                // "someone else is leader" — but it must not crash either, or
                // a brief blip becomes a restart loop across every replica.
                Err(e) => tracing::warn!(error = %e, "lease acquisition failed; retrying"),
            }
            tokio::time::sleep(self.retry_period).await;
        }
    }

    /// Keep renewing until we cannot. Returns when leadership is lost.
    async fn renew_until_lost(&self) {
        let mut last_success = Instant::now();
        loop {
            tokio::time::sleep(self.retry_period).await;

            match self.try_acquire().await {
                Ok(true) => last_success = Instant::now(),
                Ok(false) => {
                    tracing::warn!(
                        lease = %self.name,
                        "the scheduler lease was taken by another instance; stopping"
                    );
                    return;
                }
                Err(e) => {
                    // Not fatal on its own — the deadline below decides.
                    tracing::warn!(error = %e, "lease renewal failed");
                }
            }

            if last_success.elapsed() >= self.renew_deadline {
                tracing::warn!(
                    lease = %self.name,
                    deadline_secs = self.renew_deadline.as_secs(),
                    "could not renew the scheduler lease within the deadline; giving up \
                     leadership now, before another instance is entitled to take it"
                );
                return;
            }
        }
    }
}

/// Acquire leadership, run `work`, and stop it the instant leadership is lost.
///
/// `work` is only *built* after acquisition — it is a closure, not a future —
/// so a standby instance holds no watches open and costs a lease poll every
/// `retryPeriod` and nothing else.
pub async fn run_as_leader<F, Fut>(client: kube::Client, cfg: &Config, work: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if !cfg.leader_elect {
        tracing::info!(
            "leader election is disabled; scheduling immediately. Safe only if exactly one \
             instance runs — two schedulers binding the same pods is a race, not redundancy."
        );
        return work().await;
    }

    let lease = LeaderLease::new(client, cfg);
    lease.acquire().await?;

    // The work is dropped the moment renewal gives up. This is the whole
    // point: a scheduler that keeps binding after losing the lease is worse
    // than one that never had it.
    tokio::select! {
        result = work() => result,
        _ = lease.renew_until_lost() => {
            anyhow::bail!(
                "lost the scheduler lease; exiting so the service manager restarts this \
                 instance as a standby rather than leaving it scheduling without leadership"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    const FIFTEEN: Duration = Duration::from_secs(15);

    #[test]
    fn an_unheld_lease_may_be_taken() {
        assert!(may_acquire(None, None, FIFTEEN, t(100), "me"));
        assert!(may_acquire(Some(""), None, FIFTEEN, t(100), "me"));
    }

    #[test]
    fn our_own_lease_may_always_be_renewed() {
        // Including one we are late renewing — if nobody else has taken it,
        // reclaiming is correct and avoids a pointless leadership flap.
        assert!(may_acquire(Some("me"), Some(t(0)), FIFTEEN, t(10_000), "me"));
    }

    #[test]
    fn a_lease_someone_else_is_renewing_may_not_be_taken() {
        // The case that matters: taking it here means two live schedulers.
        assert!(!may_acquire(Some("other"), Some(t(100)), FIFTEEN, t(105), "me"));
    }

    #[test]
    fn a_lease_may_be_taken_only_after_the_full_duration_has_lapsed() {
        // At exactly the expiry the holder may still be alive, so the
        // comparison is strict. Being generous here is deliberate: taking
        // over early is how two schedulers both believe they are leader.
        assert!(!may_acquire(Some("other"), Some(t(100)), FIFTEEN, t(115), "me"));
        assert!(may_acquire(Some("other"), Some(t(100)), FIFTEEN, t(116), "me"));
    }

    #[test]
    fn a_held_lease_with_no_renew_time_is_treated_as_abandoned() {
        // A holder that never recorded a renewal cannot be shown to be alive,
        // and refusing to take it would deadlock the cluster permanently.
        assert!(may_acquire(Some("other"), None, FIFTEEN, t(100), "me"));
    }

    #[test]
    fn the_stop_deadline_precedes_the_takeover_deadline() {
        // The safety margin the whole scheme rests on: we give up at
        // renewDeadline, others may take over at leaseDuration, and the gap
        // between them is when nobody is scheduling. Inverting it means a
        // window where two instances both are.
        let cfg = Config::default();
        assert!(
            cfg.renew_deadline < cfg.lease_duration,
            "we must stop before anyone else may start"
        );
        assert!(cfg.retry_period < cfg.renew_deadline);
    }

    #[test]
    fn a_lease_whose_holder_is_us_under_a_different_process_is_reclaimable() {
        // Restart case: same hostname, new pid, so the identity string
        // differs. It must be treated as somebody else's — and taken only
        // once expired — or a restarted scheduler would seize a lease its own
        // still-running predecessor holds during a rolling restart.
        let mine = "host_100";
        let previous = "host_99";
        assert!(!may_acquire(Some(previous), Some(t(100)), FIFTEEN, t(105), mine));
        assert!(may_acquire(Some(previous), Some(t(100)), FIFTEEN, t(200), mine));
    }

    #[test]
    fn the_holder_identity_is_what_distinguishes_replicas() {
        // If two replicas ever shared an identity, each would see the other's
        // lease as its own and both would schedule. `may_acquire` returning
        // true for our own identity is only safe *because* the identity is
        // unique per process.
        let a = Config::default().holder_identity;
        assert!(may_acquire(Some(&a), Some(t(100)), FIFTEEN, t(101), &a));
        assert!(!may_acquire(Some(&a), Some(t(100)), FIFTEEN, t(101), "someone-else"));
    }
}
