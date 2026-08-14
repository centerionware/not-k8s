//! Configuration, read from `NODESCHEDULER_*`.
//!
//! Everything has a default, so the binary runs with no configuration at all
//! — same rule as `nodeproxy`'s config. The defaults are upstream's
//! `KubeSchedulerConfiguration` v1 defaults, because a scheduler that places
//! pods differently from kube-scheduler out of the box is not a replacement
//! for it.
//!
//! Unknown *values* are a hard error naming the accepted set, rather than a
//! warning and a fallback: a typo in `NODESCHEDULER_SCORING_STRATEGY` that
//! silently selects bin-packing instead of spreading is the kind of thing
//! nobody notices until the cluster is unbalanced and nobody can say when it
//! started.

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// Upstream's defaults, named rather than inlined so the doc comments have
/// somewhere to live.
mod defaults {
    pub const PROFILE_NAME: &str = "default-scheduler";
    /// Parallel fan-out for Filter and Score across nodes. This project
    /// targets ordinary multi-node clusters, so this is a real knob and not a
    /// formality — it is what keeps a scheduling cycle sub-linear in node
    /// count on a large cluster.
    pub const PARALLELISM: usize = 16;
    /// 0 means adaptive: score roughly `50 - nodes/125` percent of nodes,
    /// floored at 5%, so a large cluster does not pay to score every node for
    /// every pod.
    pub const PERCENTAGE_OF_NODES_TO_SCORE: i32 = 0;
    pub const POD_INITIAL_BACKOFF_SECONDS: u64 = 1;
    pub const POD_MAX_BACKOFF_SECONDS: u64 = 10;
    pub const MAX_IN_UNSCHEDULABLE_SECONDS: u64 = 300;
    pub const LEASE_DURATION_SECONDS: u64 = 15;
    pub const RENEW_DEADLINE_SECONDS: u64 = 10;
    pub const RETRY_PERIOD_SECONDS: u64 = 2;
    pub const LEASE_NAME: &str = "kube-scheduler";
    pub const LEASE_NAMESPACE: &str = "kube-system";
    /// `VolumeBinding`'s `bindTimeoutSeconds` — how long `PreBind` polls a
    /// PVC for `Bound` before giving up. Upstream's default.
    pub const VOLUME_BIND_TIMEOUT_SECONDS: u64 = 600;
}

/// How `PodTopologySpread` behaves for pods that declare no constraints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TopologyDefaulting {
    /// Upstream's default: apply the built-in `ScheduleAnyway` constraints
    /// (maxSkew 3 across zones, 5 across hosts). Costs four extra informers —
    /// Service, ReplicationController, ReplicaSet, StatefulSet — whose only
    /// purpose is deriving the label selector for these.
    #[default]
    SystemDefaulting,
    /// Apply nothing. Drops those four watches at the cost of slightly
    /// different scores for pods with no constraints of their own. Since the
    /// system defaults are `ScheduleAnyway`, this can never change which nodes
    /// are *feasible* — only their relative scores. Opt-in, because the
    /// default has to be parity.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoringStrategyKind {
    LeastAllocated,
    MostAllocated,
    RequestedToCapacityRatio,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The `spec.schedulerName` this instance answers for.
    pub profile_name: String,
    pub parallelism: usize,
    pub percentage_of_nodes_to_score: i32,
    pub pod_initial_backoff: Duration,
    pub pod_max_backoff: Duration,
    pub max_in_unschedulable: Duration,

    pub leader_elect: bool,
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
    pub lease_name: String,
    pub lease_namespace: String,
    /// Who this instance claims to be when it holds the lease. Defaults to
    /// `hostname_pid`, which is enough to tell two replicas apart and enough
    /// to find the holder when one will not give the lease up.
    pub holder_identity: String,

    pub topology_defaulting: TopologyDefaulting,
    pub scoring_strategy: ScoringStrategyKind,
    pub volume_bind_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            profile_name: defaults::PROFILE_NAME.to_string(),
            parallelism: defaults::PARALLELISM,
            percentage_of_nodes_to_score: defaults::PERCENTAGE_OF_NODES_TO_SCORE,
            pod_initial_backoff: Duration::from_secs(defaults::POD_INITIAL_BACKOFF_SECONDS),
            pod_max_backoff: Duration::from_secs(defaults::POD_MAX_BACKOFF_SECONDS),
            max_in_unschedulable: Duration::from_secs(defaults::MAX_IN_UNSCHEDULABLE_SECONDS),
            leader_elect: true,
            lease_duration: Duration::from_secs(defaults::LEASE_DURATION_SECONDS),
            renew_deadline: Duration::from_secs(defaults::RENEW_DEADLINE_SECONDS),
            retry_period: Duration::from_secs(defaults::RETRY_PERIOD_SECONDS),
            lease_name: defaults::LEASE_NAME.to_string(),
            lease_namespace: defaults::LEASE_NAMESPACE.to_string(),
            holder_identity: default_holder_identity(),
            topology_defaulting: TopologyDefaulting::SystemDefaulting,
            scoring_strategy: ScoringStrategyKind::LeastAllocated,
            volume_bind_timeout: Duration::from_secs(defaults::VOLUME_BIND_TIMEOUT_SECONDS),
        }
    }
}

fn default_holder_identity() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{host}_{}", std::process::id())
}

/// Read `name`, treating an empty value as unset.
///
/// Empty-as-unset matters because service managers routinely export a
/// variable with no value, and `"".parse::<usize>()` failing would refuse to
/// start a scheduler whose only problem is an empty `Environment=` line.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
    }
}

fn secs_env(name: &str, default: Duration) -> Result<Duration> {
    Ok(Duration::from_secs(parse_env::<u64>(name, default.as_secs())?))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let d = Config::default();

        let cfg = Config {
            profile_name: var("NODESCHEDULER_PROFILE_NAME").unwrap_or(d.profile_name),
            parallelism: parse_env("NODESCHEDULER_PARALLELISM", d.parallelism)
                .context("NODESCHEDULER_PARALLELISM must be a positive integer")?,
            percentage_of_nodes_to_score: parse_env(
                "NODESCHEDULER_PERCENTAGE_OF_NODES_TO_SCORE",
                d.percentage_of_nodes_to_score,
            )?,
            pod_initial_backoff: secs_env(
                "NODESCHEDULER_POD_INITIAL_BACKOFF_SECONDS",
                d.pod_initial_backoff,
            )?,
            pod_max_backoff: secs_env(
                "NODESCHEDULER_POD_MAX_BACKOFF_SECONDS",
                d.pod_max_backoff,
            )?,
            max_in_unschedulable: secs_env(
                "NODESCHEDULER_POD_MAX_IN_UNSCHEDULABLE_DURATION_SECONDS",
                d.max_in_unschedulable,
            )?,
            leader_elect: parse_env("NODESCHEDULER_LEADER_ELECT", d.leader_elect)?,
            lease_duration: secs_env(
                "NODESCHEDULER_LEADER_LEASE_DURATION_SECONDS",
                d.lease_duration,
            )?,
            renew_deadline: secs_env(
                "NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS",
                d.renew_deadline,
            )?,
            retry_period: secs_env(
                "NODESCHEDULER_LEADER_RETRY_PERIOD_SECONDS",
                d.retry_period,
            )?,
            lease_name: var("NODESCHEDULER_LEADER_LEASE_NAME").unwrap_or(d.lease_name),
            lease_namespace: var("NODESCHEDULER_LEADER_LEASE_NAMESPACE")
                .unwrap_or(d.lease_namespace),
            holder_identity: var("NODESCHEDULER_HOLDER_IDENTITY").unwrap_or(d.holder_identity),
            topology_defaulting: match var("NODESCHEDULER_TOPOLOGY_DEFAULTING").as_deref() {
                None | Some("SystemDefaulting") => TopologyDefaulting::SystemDefaulting,
                Some("None") => TopologyDefaulting::None,
                Some(other) => bail!(
                    "Unknown NODESCHEDULER_TOPOLOGY_DEFAULTING='{other}' \
                     (want 'SystemDefaulting' — upstream's default — or 'None')."
                ),
            },
            scoring_strategy: match var("NODESCHEDULER_SCORING_STRATEGY").as_deref() {
                None | Some("LeastAllocated") => ScoringStrategyKind::LeastAllocated,
                Some("MostAllocated") => ScoringStrategyKind::MostAllocated,
                Some("RequestedToCapacityRatio") => {
                    ScoringStrategyKind::RequestedToCapacityRatio
                }
                Some(other) => bail!(
                    "Unknown NODESCHEDULER_SCORING_STRATEGY='{other}' \
                     (want 'LeastAllocated' — the default — 'MostAllocated', or \
                     'RequestedToCapacityRatio')."
                ),
            },
            volume_bind_timeout: secs_env(
                "NODESCHEDULER_VOLUME_BIND_TIMEOUT_SECONDS",
                d.volume_bind_timeout,
            )?,
        };

        cfg.validate()?;
        cfg.log_summary();
        Ok(cfg)
    }

    /// Cross-field checks.
    ///
    /// The leader-election inequalities are the ones worth enforcing: get them
    /// wrong and the failure is not a startup error but a scheduler that
    /// intermittently loses a lease it still holds, or two schedulers that
    /// both believe they are leader — which is the double-binding race the
    /// whole mechanism exists to prevent.
    fn validate(&self) -> Result<()> {
        if self.parallelism == 0 {
            bail!("NODESCHEDULER_PARALLELISM must be at least 1.");
        }
        if !(0..=100).contains(&self.percentage_of_nodes_to_score) {
            bail!(
                "NODESCHEDULER_PERCENTAGE_OF_NODES_TO_SCORE must be 0-100 \
                 (0 means adaptive), got {}.",
                self.percentage_of_nodes_to_score
            );
        }
        if self.pod_initial_backoff > self.pod_max_backoff {
            bail!(
                "NODESCHEDULER_POD_INITIAL_BACKOFF_SECONDS ({}s) must not exceed \
                 NODESCHEDULER_POD_MAX_BACKOFF_SECONDS ({}s).",
                self.pod_initial_backoff.as_secs(),
                self.pod_max_backoff.as_secs()
            );
        }
        if self.leader_elect {
            if self.renew_deadline >= self.lease_duration {
                bail!(
                    "NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS ({}s) must be less than \
                     NODESCHEDULER_LEADER_LEASE_DURATION_SECONDS ({}s), or this instance \
                     would still believe it holds a lease another has already taken.",
                    self.renew_deadline.as_secs(),
                    self.lease_duration.as_secs()
                );
            }
            if self.retry_period >= self.renew_deadline {
                bail!(
                    "NODESCHEDULER_LEADER_RETRY_PERIOD_SECONDS ({}s) must be less than \
                     NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS ({}s), or a single failed \
                     renewal would lose the lease with no attempt to retry it.",
                    self.retry_period.as_secs(),
                    self.renew_deadline.as_secs()
                );
            }
        }
        Ok(())
    }

    fn log_summary(&self) {
        tracing::info!(
            profile = %self.profile_name,
            parallelism = self.parallelism,
            percentage_of_nodes_to_score = self.percentage_of_nodes_to_score,
            leader_elect = self.leader_elect,
            lease = %format!("{}/{}", self.lease_namespace, self.lease_name),
            holder = %self.holder_identity,
            topology_defaulting = ?self.topology_defaulting,
            scoring_strategy = ?self.scoring_strategy,
            "nodescheduler configuration"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env vars are process-global, so these tests cannot run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard(Vec<&'static str>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.0 {
                std::env::remove_var(k);
            }
        }
    }

    fn with_env<T>(vars: &[(&'static str, &str)], f: impl FnOnce() -> T) -> T {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard(vars.iter().map(|(k, _)| *k).collect());
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        f()
    }

    #[test]
    fn an_unconfigured_scheduler_gets_upstreams_defaults() {
        // A drop-in replacement has to behave like the thing it replaces
        // before anyone touches a knob.
        let c = with_env(&[], || Config::from_env().unwrap());

        assert_eq!(c.profile_name, "default-scheduler");
        assert_eq!(c.parallelism, 16);
        assert_eq!(c.percentage_of_nodes_to_score, 0);
        assert_eq!(c.pod_initial_backoff, Duration::from_secs(1));
        assert_eq!(c.pod_max_backoff, Duration::from_secs(10));
        assert_eq!(c.max_in_unschedulable, Duration::from_secs(300));
        assert_eq!(c.lease_duration, Duration::from_secs(15));
        assert_eq!(c.renew_deadline, Duration::from_secs(10));
        assert_eq!(c.retry_period, Duration::from_secs(2));
        assert_eq!(c.lease_name, "kube-scheduler");
        assert_eq!(c.lease_namespace, "kube-system");
        assert!(c.leader_elect);
    }

    #[test]
    fn an_empty_value_is_treated_as_unset() {
        // Service managers routinely export empty variables; refusing to start
        // over one would be a needless outage.
        let c = with_env(&[("NODESCHEDULER_PARALLELISM", "")], || {
            Config::from_env().unwrap()
        });
        assert_eq!(c.parallelism, 16);
    }

    #[test]
    fn an_unknown_topology_defaulting_value_is_refused_by_name() {
        let err = with_env(&[("NODESCHEDULER_TOPOLOGY_DEFAULTING", "sure")], || {
            Config::from_env().unwrap_err().to_string()
        });
        assert!(err.contains("SystemDefaulting"), "the error must name the accepted set: {err}");
    }

    #[test]
    fn an_unknown_scoring_strategy_is_refused_rather_than_silently_defaulted() {
        // A typo here silently switching spreading to bin-packing is the kind
        // of thing nobody notices until the cluster is lopsided.
        let err = with_env(&[("NODESCHEDULER_SCORING_STRATEGY", "MostAllocted")], || {
            Config::from_env().unwrap_err().to_string()
        });
        assert!(err.contains("MostAllocated"));
    }

    #[test]
    fn a_renew_deadline_at_or_past_the_lease_duration_is_refused() {
        // Otherwise this instance keeps binding pods after another replica has
        // legitimately taken the lease.
        let err = with_env(
            &[
                ("NODESCHEDULER_LEADER_LEASE_DURATION_SECONDS", "10"),
                ("NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS", "10"),
            ],
            || Config::from_env().unwrap_err().to_string(),
        );
        assert!(err.contains("must be less than"));
    }

    #[test]
    fn a_retry_period_at_or_past_the_renew_deadline_is_refused() {
        let err = with_env(
            &[
                ("NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS", "5"),
                ("NODESCHEDULER_LEADER_RETRY_PERIOD_SECONDS", "5"),
            ],
            || Config::from_env().unwrap_err().to_string(),
        );
        assert!(err.contains("retry"), "{err}");
    }

    #[test]
    fn the_leader_election_inequalities_are_not_checked_when_it_is_disabled() {
        // A single-instance deployment should not be blocked by values it
        // will never use.
        let c = with_env(
            &[
                ("NODESCHEDULER_LEADER_ELECT", "false"),
                ("NODESCHEDULER_LEADER_RENEW_DEADLINE_SECONDS", "999"),
            ],
            || Config::from_env().unwrap(),
        );
        assert!(!c.leader_elect);
    }

    #[test]
    fn zero_parallelism_is_refused() {
        let err = with_env(&[("NODESCHEDULER_PARALLELISM", "0")], || {
            Config::from_env().unwrap_err().to_string()
        });
        assert!(err.contains("at least 1"));
    }

    #[test]
    fn a_percentage_outside_zero_to_a_hundred_is_refused() {
        let err = with_env(&[("NODESCHEDULER_PERCENTAGE_OF_NODES_TO_SCORE", "150")], || {
            Config::from_env().unwrap_err().to_string()
        });
        assert!(err.contains("0-100"));
    }

    #[test]
    fn an_initial_backoff_above_the_maximum_is_refused() {
        let err = with_env(
            &[
                ("NODESCHEDULER_POD_INITIAL_BACKOFF_SECONDS", "60"),
                ("NODESCHEDULER_POD_MAX_BACKOFF_SECONDS", "10"),
            ],
            || Config::from_env().unwrap_err().to_string(),
        );
        assert!(err.contains("must not exceed"));
    }

    #[test]
    fn a_custom_profile_name_is_honoured() {
        // How a second scheduler runs alongside the default one.
        let c = with_env(&[("NODESCHEDULER_PROFILE_NAME", "batch-scheduler")], || {
            Config::from_env().unwrap()
        });
        assert_eq!(c.profile_name, "batch-scheduler");
    }

    #[test]
    fn a_non_numeric_duration_is_refused_naming_the_variable() {
        let err = with_env(&[("NODESCHEDULER_POD_MAX_BACKOFF_SECONDS", "ten")], || {
            Config::from_env().unwrap_err().to_string()
        });
        assert!(err.contains("NODESCHEDULER_POD_MAX_BACKOFF_SECONDS"), "{err}");
    }

    #[test]
    fn the_default_holder_identity_distinguishes_two_replicas() {
        // Two schedulers sharing an identity would each think they already
        // hold the lease.
        let id = default_holder_identity();
        assert!(id.contains('_'));
        assert!(id.ends_with(&std::process::id().to_string()));
    }
}
