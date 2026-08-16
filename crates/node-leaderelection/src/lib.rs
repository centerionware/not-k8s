//! Leader election over a `coordination.k8s.io/v1` Lease.
//!
//! Extracted from `nodescheduler`'s original `election.rs` (nothing in that
//! file was actually scheduler-specific — see git history on that module for
//! the original reasoning). `nodecontroller` is the second consumer, and a
//! second copy of ~250 lines of optimistic-concurrency Lease logic is exactly
//! the kind of duplication this project's own house style avoids elsewhere
//! (`crates/nodeproxy`'s Cargo.toml comment on not sharing nodelet's
//! dependency tree is the same principle in the other direction — a boundary
//! enforced by *not* copying, not by copying and hoping the two stay in
//! sync).
//!
//! # Why optimistic concurrency, not server-side-apply `.force()`
//!
//! `nodelet` already writes a Lease, and it is the wrong shape to borrow for
//! this. That one is a node-liveness heartbeat written with server-side apply
//! and `.force()` — i.e. "this record is mine, overwrite whatever is there".
//! That is exactly correct for a heartbeat, where a node is the sole author
//! of its own liveness, and exactly wrong here, where the entire purpose is
//! to find out whether someone else got there first.
//!
//! So this uses optimistic concurrency instead: read the Lease, decide, and
//! write it back with the `resourceVersion` we read. If another replica wrote
//! in between, the apiserver rejects the update with a conflict and we lost
//! the race — which is the answer we were asking for, not an error to retry
//! blindly.
//!
//! # What a single-writer component needs it for
//!
//! Both `nodescheduler` (single writer of `Binding`) and `nodecontroller`
//! (single writer of, e.g., a Node's lifecycle taints or its `podCIDR`) share
//! the same hazard: two replicas racing the same decision don't just produce
//! duplicate work, they can each *assume* their own decision is the only one
//! and leave in-memory state that disagrees with what the other replica also
//! just committed. This project targets clusters with several control-plane
//! nodes, so that is a live concern rather than a theoretical one.
//!
//! # Losing the lease must stop the work immediately
//!
//! [`run_as_leader`] runs the caller's future *inside* a `select!` against
//! the renewal loop, so the moment renewal fails past the deadline the work
//! is dropped. A component that keeps acting after losing leadership is
//! precisely the race the lease exists to prevent — worse than having no
//! lease at all, because the other replica is now also acting and neither
//! knows.
//!
//! The timing rule is `retryPeriod < renewDeadline < leaseDuration`, checked
//! by [`ElectionConfig::validate`]. The gap between giving up (`renewDeadline`)
//! and another replica being allowed to take over (`leaseDuration`) is the
//! safety margin: a leader stops before anyone else may start.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::{Span, Timestamp};
use kube::api::{Api, PostParams};
use std::time::{Duration, Instant};

/// Everything a [`LeaderLease`] needs, decoupled from any one component's own
/// `Config` type so this crate has no dependency in either direction on the
/// components that use it.
#[derive(Clone, Debug)]
pub struct ElectionConfig {
    pub enabled: bool,
    pub lease_name: String,
    pub lease_namespace: String,
    /// Who this instance claims to be when it holds the lease. Enough to
    /// tell two replicas apart and to find the holder when one won't give
    /// the lease up — a hostname is not unique enough on its own if a
    /// replica restarts under the same hostname, so callers should include
    /// something per-process (a pid) too.
    pub holder_identity: String,
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
}

impl ElectionConfig {
    /// Cross-field checks. Get these wrong and the failure is not a startup
    /// error but a leader that intermittently loses a lease it still holds,
    /// or two leaders that both believe they're it — the double-writer race
    /// this whole mechanism exists to prevent.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.renew_deadline >= self.lease_duration {
            bail!(
                "leader-election renew_deadline ({}s) must be less than lease_duration ({}s), \
                 or this instance would still believe it holds a lease another has already \
                 taken.",
                self.renew_deadline.as_secs(),
                self.lease_duration.as_secs()
            );
        }
        if self.retry_period >= self.renew_deadline {
            bail!(
                "leader-election retry_period ({}s) must be less than renew_deadline ({}s), or \
                 a single failed renewal would lose the lease with no attempt to retry it.",
                self.retry_period.as_secs(),
                self.renew_deadline.as_secs()
            );
        }
        Ok(())
    }
}

/// Whether we may take a lease, given what is currently recorded in it.
///
/// Pure so the decision is testable without an apiserver.
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
    // than the full lease duration. Deliberately generous — taking over
    // early is how two leaders end up both believing they are leader.
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
    pub fn new(client: kube::Client, cfg: &ElectionConfig) -> Self {
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

        match self
            .api
            .get_opt(&self.name)
            .await
            .context("reading the leader-election lease")?
        {
            None => {
                // First replica in a fresh cluster. A create that loses the
                // race returns AlreadyExists, which is simply "not us".
                let lease = self.lease_object(None, now, now, 1);
                match self.api.create(&PostParams::default(), &lease).await {
                    Ok(_) => Ok(true),
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                    Err(e) => Err(e).context("creating the leader-election lease"),
                }
            }
            Some(existing) => {
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

                // A transition is a change of holder, not a renewal by the
                // same one — the field an operator reads to see whether
                // leadership has been flapping.
                let transitions = spec.lease_transitions.unwrap_or(0)
                    + i32::from(holder.as_deref() != Some(self.identity.as_str()));

                // A renewal by the same holder must keep the original
                // acquireTime — it records when *this* leadership term
                // started. Only a genuine change of holder gets a fresh one.
                let acquired_at = if holder.as_deref() == Some(self.identity.as_str()) {
                    spec.acquire_time.map(|t| t.0).unwrap_or(now)
                } else {
                    now
                };

                let mut lease = self.lease_object(
                    existing.metadata.resource_version.clone(),
                    now,
                    acquired_at,
                    transitions,
                );
                lease.metadata.name = Some(self.name.clone());

                // The optimistic-concurrency write: carrying the
                // resourceVersion we read is what turns a lost race into a
                // 409 instead of a silent double-leader.
                match self
                    .api
                    .replace(&self.name, &PostParams::default(), &lease)
                    .await
                {
                    Ok(_) => Ok(true),
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                    Err(e) => Err(e).context("acquiring the leader-election lease"),
                }
            }
        }
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
                        "acquired leadership"
                    );
                    return Ok(());
                }
                Ok(false) => {
                    if !announced {
                        tracing::info!(
                            lease = %self.name,
                            "another instance holds the lease; standing by"
                        );
                        announced = true;
                    }
                }
                // A transient apiserver problem must not be mistaken for
                // "someone else is leader" — but must not crash either, or a
                // brief blip becomes a restart loop across every replica.
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

            // A blackholed apiserver must not prevent the deadline check from
            // running. Treat an attempt that lasts a full retry period as a
            // failed renewal and continue to the normal deadline decision.
            match tokio::time::timeout(self.retry_period, self.try_acquire()).await {
                Ok(Ok(true)) => last_success = Instant::now(),
                Ok(Ok(false)) => {
                    tracing::warn!(lease = %self.name, "the lease was taken by another instance; stopping");
                    return;
                }
                Ok(Err(e)) => {
                    // Not fatal on its own — the deadline below decides.
                    tracing::warn!(error = %e, "lease renewal failed");
                }
                Err(_) => {
                    tracing::warn!(lease = %self.name, "lease renewal timed out");
                }
            }

            if last_success.elapsed() >= self.renew_deadline {
                tracing::warn!(
                    lease = %self.name,
                    deadline_secs = self.renew_deadline.as_secs(),
                    "could not renew the lease within the deadline; giving up leadership now, \
                     before another instance is entitled to take it"
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
/// `retry_period` and nothing else.
pub async fn run_as_leader<F, Fut>(
    client: kube::Client,
    cfg: &ElectionConfig,
    work: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    cfg.validate()?;

    if !cfg.enabled {
        tracing::info!(
            "leader election is disabled; starting immediately. Safe only if exactly one \
             instance runs — two active writers is a race, not redundancy."
        );
        return work().await;
    }

    let lease = LeaderLease::new(client, cfg);
    lease.acquire().await?;

    // The work is dropped the moment renewal gives up. A leader that keeps
    // acting after losing the lease is worse than one that never had it.
    tokio::select! {
        result = work() => result,
        _ = lease.renew_until_lost() => {
            anyhow::bail!(
                "lost the leader-election lease; exiting so the service manager restarts this \
                 instance as a standby rather than leaving it acting without leadership"
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

    fn test_cfg() -> ElectionConfig {
        ElectionConfig {
            enabled: true,
            lease_name: "test".to_string(),
            lease_namespace: "kube-system".to_string(),
            holder_identity: "host_100".to_string(),
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
        }
    }

    #[test]
    fn an_unheld_lease_may_be_taken() {
        assert!(may_acquire(None, None, FIFTEEN, t(100), "me"));
        assert!(may_acquire(Some(""), None, FIFTEEN, t(100), "me"));
    }

    #[test]
    fn our_own_lease_may_always_be_renewed() {
        assert!(may_acquire(
            Some("me"),
            Some(t(0)),
            FIFTEEN,
            t(10_000),
            "me"
        ));
    }

    #[test]
    fn a_lease_someone_else_is_renewing_may_not_be_taken() {
        assert!(!may_acquire(
            Some("other"),
            Some(t(100)),
            FIFTEEN,
            t(105),
            "me"
        ));
    }

    #[test]
    fn a_lease_may_be_taken_only_after_the_full_duration_has_lapsed() {
        assert!(!may_acquire(
            Some("other"),
            Some(t(100)),
            FIFTEEN,
            t(115),
            "me"
        ));
        assert!(may_acquire(
            Some("other"),
            Some(t(100)),
            FIFTEEN,
            t(116),
            "me"
        ));
    }

    #[test]
    fn a_held_lease_with_no_renew_time_is_treated_as_abandoned() {
        assert!(may_acquire(Some("other"), None, FIFTEEN, t(100), "me"));
    }

    #[test]
    fn the_stop_deadline_must_precede_the_takeover_deadline() {
        let cfg = test_cfg();
        assert!(cfg.validate().is_ok());
        assert!(cfg.renew_deadline < cfg.lease_duration);
        assert!(cfg.retry_period < cfg.renew_deadline);
    }

    #[test]
    fn validate_rejects_a_renew_deadline_that_does_not_precede_lease_duration() {
        let mut cfg = test_cfg();
        cfg.renew_deadline = cfg.lease_duration;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_a_retry_period_that_does_not_precede_renew_deadline() {
        let mut cfg = test_cfg();
        cfg.retry_period = cfg.renew_deadline;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_skips_the_timing_checks_when_election_is_disabled() {
        let mut cfg = test_cfg();
        cfg.enabled = false;
        cfg.renew_deadline = cfg.lease_duration; // would otherwise fail
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn a_lease_whose_holder_is_us_under_a_different_process_is_reclaimable() {
        let mine = "host_100";
        let previous = "host_99";
        assert!(!may_acquire(
            Some(previous),
            Some(t(100)),
            FIFTEEN,
            t(105),
            mine
        ));
        assert!(may_acquire(
            Some(previous),
            Some(t(100)),
            FIFTEEN,
            t(200),
            mine
        ));
    }

    #[test]
    fn the_holder_identity_is_what_distinguishes_replicas() {
        let a = test_cfg().holder_identity;
        assert!(may_acquire(Some(&a), Some(t(100)), FIFTEEN, t(101), &a));
        assert!(!may_acquire(
            Some(&a),
            Some(t(100)),
            FIFTEEN,
            t(101),
            "someone-else"
        ));
    }
}
