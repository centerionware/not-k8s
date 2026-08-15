//! Configuration, read from `NODECONTROLLER_*`.
//!
//! Everything has a default, so the binary runs with no configuration at all
//! — same rule as `nodescheduler`'s and `nodeproxy`'s config. Defaults are
//! upstream kube-controller-manager's own defaults (node monitor grace
//! period, node CIDR mask size, lease timings) or, where this project has
//! already made that choice for the rest of the control plane
//! (`--cluster-cidr=10.42.0.0/16` in `deploy/setup-control-plane.sh`), the
//! same value — a controller manager that allocates pod CIDRs out of a
//! different range than the rest of the cluster expects is not a
//! replacement for it.

use anyhow::{bail, Result};
use std::time::Duration;

mod defaults {
    /// Matches `deploy/setup-control-plane.sh`'s `CLUSTER_CIDR` default —
    /// see this module's own doc comment for why these two must agree.
    pub const CLUSTER_CIDR: &str = "10.42.0.0/16";
    /// Upstream's `--node-cidr-mask-size` default for an IPv4 cluster CIDR.
    pub const NODE_CIDR_MASK_SIZE: u8 = 24;
    /// Upstream's `--node-monitor-grace-period`: how long a Node may go
    /// without a heartbeat before node-lifecycle taints it.
    pub const NODE_MONITOR_GRACE_PERIOD_SECONDS: u64 = 40;

    pub const LEASE_DURATION_SECONDS: u64 = 15;
    pub const RENEW_DEADLINE_SECONDS: u64 = 10;
    pub const RETRY_PERIOD_SECONDS: u64 = 2;
    /// Upstream's own lease name/namespace for kube-controller-manager —
    /// reused deliberately rather than inventing a new one: operator
    /// tooling that already reads `kube-system/kube-controller-manager`'s
    /// Lease to see who's active keeps working unmodified. Safe to share
    /// the name because this project always pairs
    /// `CONTROLLER_MANAGER=nodecontroller` with k3s's own
    /// `--disable-controller-manager` — the two are never both live.
    pub const LEASE_NAME: &str = "kube-controller-manager";
    pub const LEASE_NAMESPACE: &str = "kube-system";

    /// The pacing governor's tick period — see docs/CONTROLLER_MANAGER.md's
    /// "CPU-budgeted governor" section. 100ms: coarse enough that the
    /// governor's own bookkeeping is negligible next to the budget it's
    /// enforcing, fine enough that a burst is smoothed over sub-second time
    /// rather than visibly stalling.
    pub const TICK_PERIOD_MILLIS: u64 = 100;
    /// Target CPU spend per tick, as a percent of one core. Mid-point of
    /// the stated 0.3-1% target range.
    pub const CPU_BUDGET_PERCENT: f64 = 0.6;
    /// Fraction of a deadline's own interval to jitter by on insert, so
    /// correlated deadlines (every Node renews on the same period) don't
    /// land in the same wheel slot. Matches the reasoning kubelet's own
    /// sync loop jitter uses.
    pub const JITTER_FRACTION: f64 = 0.05;
}

#[derive(Clone, Debug)]
pub struct Config {
    pub cluster_cidr: String,
    pub node_cidr_mask_size: u8,
    pub node_monitor_grace_period: Duration,

    pub leader_elect: bool,
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
    pub lease_name: String,
    pub lease_namespace: String,
    pub holder_identity: String,

    pub tick_period: Duration,
    pub cpu_budget_percent: f64,
    pub jitter_fraction: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cluster_cidr: defaults::CLUSTER_CIDR.to_string(),
            node_cidr_mask_size: defaults::NODE_CIDR_MASK_SIZE,
            node_monitor_grace_period: Duration::from_secs(
                defaults::NODE_MONITOR_GRACE_PERIOD_SECONDS,
            ),
            leader_elect: true,
            lease_duration: Duration::from_secs(defaults::LEASE_DURATION_SECONDS),
            renew_deadline: Duration::from_secs(defaults::RENEW_DEADLINE_SECONDS),
            retry_period: Duration::from_secs(defaults::RETRY_PERIOD_SECONDS),
            lease_name: defaults::LEASE_NAME.to_string(),
            lease_namespace: defaults::LEASE_NAMESPACE.to_string(),
            holder_identity: default_holder_identity(),
            tick_period: Duration::from_millis(defaults::TICK_PERIOD_MILLIS),
            cpu_budget_percent: defaults::CPU_BUDGET_PERCENT,
            jitter_fraction: defaults::JITTER_FRACTION,
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

/// Read `name`, treating an empty value as unset — a service manager
/// routinely exports a variable with no value, and a strict parse failing on
/// that would refuse to start over an effectively-unset knob.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v.parse::<T>().map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
    }
}

fn secs_env(name: &str, default: Duration) -> Result<Duration> {
    Ok(Duration::from_secs(parse_env::<u64>(name, default.as_secs())?))
}

fn millis_env(name: &str, default: Duration) -> Result<Duration> {
    Ok(Duration::from_millis(parse_env::<u64>(
        name,
        default.as_millis() as u64,
    )?))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let d = Config::default();

        let cfg = Config {
            cluster_cidr: var("NODECONTROLLER_CLUSTER_CIDR").unwrap_or(d.cluster_cidr),
            node_cidr_mask_size: parse_env(
                "NODECONTROLLER_NODE_CIDR_MASK_SIZE",
                d.node_cidr_mask_size,
            )?,
            node_monitor_grace_period: secs_env(
                "NODECONTROLLER_NODE_MONITOR_GRACE_PERIOD_SECONDS",
                d.node_monitor_grace_period,
            )?,
            leader_elect: parse_env("NODECONTROLLER_LEADER_ELECT", d.leader_elect)?,
            lease_duration: secs_env(
                "NODECONTROLLER_LEADER_LEASE_DURATION_SECONDS",
                d.lease_duration,
            )?,
            renew_deadline: secs_env(
                "NODECONTROLLER_LEADER_RENEW_DEADLINE_SECONDS",
                d.renew_deadline,
            )?,
            retry_period: secs_env(
                "NODECONTROLLER_LEADER_RETRY_PERIOD_SECONDS",
                d.retry_period,
            )?,
            lease_name: var("NODECONTROLLER_LEADER_LEASE_NAME").unwrap_or(d.lease_name),
            lease_namespace: var("NODECONTROLLER_LEADER_LEASE_NAMESPACE")
                .unwrap_or(d.lease_namespace),
            holder_identity: var("NODECONTROLLER_HOLDER_IDENTITY").unwrap_or(d.holder_identity),
            tick_period: millis_env("NODECONTROLLER_TICK_PERIOD_MILLIS", d.tick_period)?,
            cpu_budget_percent: parse_env(
                "NODECONTROLLER_CPU_BUDGET_PERCENT",
                d.cpu_budget_percent,
            )?,
            jitter_fraction: parse_env("NODECONTROLLER_JITTER_FRACTION", d.jitter_fraction)?,
        };

        cfg.validate()?;
        cfg.log_summary();
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.node_cidr_mask_size) {
            bail!(
                "NODECONTROLLER_NODE_CIDR_MASK_SIZE must be 1-32, got {}.",
                self.node_cidr_mask_size
            );
        }
        if self.cpu_budget_percent <= 0.0 || self.cpu_budget_percent > 100.0 {
            bail!(
                "NODECONTROLLER_CPU_BUDGET_PERCENT must be >0 and <=100, got {}.",
                self.cpu_budget_percent
            );
        }
        if !(0.0..1.0).contains(&self.jitter_fraction) {
            bail!(
                "NODECONTROLLER_JITTER_FRACTION must be >=0 and <1, got {}.",
                self.jitter_fraction
            );
        }
        if self.tick_period.is_zero() {
            bail!("NODECONTROLLER_TICK_PERIOD_MILLIS must be at least 1.");
        }
        if self.leader_elect {
            if self.renew_deadline >= self.lease_duration {
                bail!(
                    "NODECONTROLLER_LEADER_RENEW_DEADLINE_SECONDS ({}s) must be less than \
                     NODECONTROLLER_LEADER_LEASE_DURATION_SECONDS ({}s), or this instance would \
                     still believe it holds a lease another has already taken.",
                    self.renew_deadline.as_secs(),
                    self.lease_duration.as_secs()
                );
            }
            if self.retry_period >= self.renew_deadline {
                bail!(
                    "NODECONTROLLER_LEADER_RETRY_PERIOD_SECONDS ({}s) must be less than \
                     NODECONTROLLER_LEADER_RENEW_DEADLINE_SECONDS ({}s), or a single failed \
                     renewal would lose the lease with no attempt to retry it.",
                    self.retry_period.as_secs(),
                    self.renew_deadline.as_secs()
                );
            }
        }
        Ok(())
    }

    pub fn election(&self) -> node_leaderelection::ElectionConfig {
        node_leaderelection::ElectionConfig {
            enabled: self.leader_elect,
            lease_name: self.lease_name.clone(),
            lease_namespace: self.lease_namespace.clone(),
            holder_identity: self.holder_identity.clone(),
            lease_duration: self.lease_duration,
            renew_deadline: self.renew_deadline,
            retry_period: self.retry_period,
        }
    }

    fn log_summary(&self) {
        tracing::info!(
            cluster_cidr = %self.cluster_cidr,
            node_cidr_mask_size = self.node_cidr_mask_size,
            node_monitor_grace_period_secs = self.node_monitor_grace_period.as_secs(),
            leader_elect = self.leader_elect,
            lease = %format!("{}/{}", self.lease_namespace, self.lease_name),
            tick_period_ms = self.tick_period.as_millis() as u64,
            cpu_budget_percent = self.cpu_budget_percent,
            "nodecontroller starting"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn rejects_an_out_of_range_mask_size() {
        let mut cfg = Config::default();
        cfg.node_cidr_mask_size = 33;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_a_non_positive_cpu_budget() {
        let mut cfg = Config::default();
        cfg.cpu_budget_percent = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_leader_election_timings_that_invert() {
        let mut cfg = Config::default();
        cfg.renew_deadline = cfg.lease_duration;
        assert!(cfg.validate().is_err());
    }
}
