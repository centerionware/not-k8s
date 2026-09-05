//! Plugin verdicts.
//!
//! # The distinction that carries the most weight in this crate
//!
//! [`Code::Unschedulable`] and [`Code::UnschedulableAndUnresolvable`] look like
//! a severity pair and are not. They are three separate switches:
//!
//!   * whether `PostFilter` (i.e. preemption) is attempted for the pod at all,
//!   * whether a given *node* is a preemption candidate,
//!   * which plugin gets recorded for requeue purposes, and therefore which
//!     cluster events can ever un-stick the pod.
//!
//! "Unresolvable" means *no amount of evicting other pods would help* — the
//! node's name is wrong, its taints don't match, the pod's affinity excludes
//! it. Killing something to make room would be pointless. "Unschedulable"
//! means it doesn't fit *right now*, which preemption can fix.
//!
//! Getting this backwards produces one of two silent failures: pods that never
//! preempt when they should, or preemption churn that evicts victims and then
//! still fails to place the preemptor.

use std::fmt;

/// Why a plugin returned what it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// Carry on.
    Success,
    /// The plugin itself failed — a bug, or an unreachable dependency. Aborts
    /// the cycle and is retried; never interpreted as a placement decision.
    Error,
    /// This pod does not fit here *now*. Preemption may be able to fix it.
    Unschedulable,
    /// This pod cannot fit here, and evicting other pods would not change
    /// that. Excludes the node from preemption candidacy.
    UnschedulableAndUnresolvable,
    /// This plugin has nothing to say for this pod; skip its later extension
    /// points this cycle. Returned from PreFilter/PreScore as a pure
    /// optimisation — it must never change the outcome.
    Skip,
    /// Permit only: hold the pod in the binding cycle until approved, denied,
    /// or timed out.
    Wait,
    /// Rejected, but real progress was made, so don't charge the pod backoff.
    /// A pod rejected this way goes straight back to the active queue rather
    /// than serving a penalty. DRA is the motivating case: allocating a claim
    /// is progress even though this cycle didn't place the pod.
    Pending,
    /// The object disappeared or entered deletion while an asynchronous bind
    /// was still in flight. This is terminal for this bind attempt: retrying
    /// it cannot place the object and only creates a conflict storm.
    Cancelled,
}

impl Code {
    /// Whether this verdict rejects the pod (as opposed to an internal error
    /// or a no-op).
    pub fn is_rejection(self) -> bool {
        matches!(
            self,
            Code::Unschedulable | Code::UnschedulableAndUnresolvable | Code::Pending
        )
    }

    /// Whether preemption could conceivably help. The single question
    /// `PostFilter` and preemption-candidate selection both ask.
    pub fn is_resolvable_by_preemption(self) -> bool {
        matches!(self, Code::Unschedulable)
    }
}

/// A plugin's verdict, with the reasons a human needs to understand a Pending
/// pod. Reasons end up in the `PodScheduled=False` condition and the event,
/// which for most users is the only visible output this component has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub code: Code,
    /// The plugin that produced this. Empty for `Success`.
    pub plugin: &'static str,
    pub reasons: Vec<String>,
}

impl Status {
    pub const fn success() -> Self {
        Status { code: Code::Success, plugin: "", reasons: Vec::new() }
    }

    pub const fn skip() -> Self {
        Status { code: Code::Skip, plugin: "", reasons: Vec::new() }
    }

    pub fn unschedulable(plugin: &'static str, reason: impl Into<String>) -> Self {
        Status { code: Code::Unschedulable, plugin, reasons: vec![reason.into()] }
    }

    pub fn unresolvable(plugin: &'static str, reason: impl Into<String>) -> Self {
        Status {
            code: Code::UnschedulableAndUnresolvable,
            plugin,
            reasons: vec![reason.into()],
        }
    }

    pub fn error(plugin: &'static str, reason: impl Into<String>) -> Self {
        Status { code: Code::Error, plugin, reasons: vec![reason.into()] }
    }

    pub fn pending(plugin: &'static str, reason: impl Into<String>) -> Self {
        Status { code: Code::Pending, plugin, reasons: vec![reason.into()] }
    }

    pub fn cancelled(plugin: &'static str, reason: impl Into<String>) -> Self {
        Status { code: Code::Cancelled, plugin, reasons: vec![reason.into()] }
    }

    pub fn is_success(&self) -> bool {
        self.code == Code::Success
    }

    pub fn is_skip(&self) -> bool {
        self.code == Code::Skip
    }

    pub fn is_rejection(&self) -> bool {
        self.code.is_rejection()
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.plugin.is_empty() {
            write!(f, "{:?}", self.code)
        } else {
            write!(f, "{}: {}", self.plugin, self.reasons.join(", "))
        }
    }
}

/// Why every node was rejected, kept per scheduling attempt.
///
/// This is what makes `kubectl describe pod` legible ("0/5 nodes are
/// available: 3 Insufficient cpu, 2 node(s) had untolerated taint"), so the
/// per-node reasons are aggregated by message rather than listed per node.
#[derive(Clone, Debug, Default)]
pub struct NodeToStatus {
    per_node: Vec<(String, Status)>,
}

impl NodeToStatus {
    pub fn record(&mut self, node: impl Into<String>, status: Status) {
        self.per_node.push((node.into(), status));
    }

    pub fn len(&self) -> usize {
        self.per_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.per_node.is_empty()
    }

    /// Any plugin malfunction aborts the entire cycle, even if another node
    /// happened to pass. Treating Error as one node's ordinary rejection can
    /// produce a placement from incomplete or corrupt plugin state.
    pub fn first_error(&self) -> Option<&Status> {
        self.per_node.iter().map(|(_, status)| status).find(|s| s.code == Code::Error)
    }

    /// Nodes preemption could plausibly free up — i.e. rejected with
    /// `Unschedulable`, not `UnschedulableAndUnresolvable`.
    pub fn preemption_candidates(&self) -> Vec<&str> {
        self.per_node
            .iter()
            .filter(|(_, s)| s.code.is_resolvable_by_preemption())
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// The most recent verdict recorded for one node. Filter normally writes
    /// exactly once per node; extenders may replace it, and the last writer is
    /// the verdict the cycle ultimately acted on.
    pub fn for_node(&self, node: &str) -> Option<&Status> {
        self.per_node.iter().rev().find(|(name, _)| name == node).map(|(_, status)| status)
    }

    /// The plugins that rejected this pod anywhere — the set whose registered
    /// events can requeue it. Missing an entry here is a silent stall, so
    /// this is deliberately the union across all nodes rather than the
    /// reasons for any one node.
    pub fn rejecting_plugins(&self) -> Vec<&'static str> {
        let mut plugins: Vec<&'static str> = self
            .per_node
            .iter()
            .filter(|(_, s)| s.is_rejection())
            .map(|(_, s)| s.plugin)
            .collect();
        plugins.sort_unstable();
        plugins.dedup();
        plugins
    }

    /// `kubectl`-shaped summary: counts of each distinct reason.
    pub fn summary(&self, total_nodes: usize) -> String {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for (_, s) in &self.per_node {
            // A distinct set, not the raw reasons list: a plugin reporting
            // two reasons for the same node must still count that node
            // once, or the rendered total can claim more rejected nodes
            // than the cluster has.
            let distinct: std::collections::BTreeSet<&String> = s.reasons.iter().collect();
            for r in distinct {
                *counts.entry(r.clone()).or_default() += 1;
            }
        }
        let detail = counts
            .iter()
            .map(|(reason, n)| format!("{n} {reason}"))
            .collect::<Vec<_>>()
            .join(", ");
        if detail.is_empty() {
            return format!("0/{total_nodes} nodes are available");
        }
        format!("0/{total_nodes} nodes are available: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_unschedulable_invites_preemption() {
        assert!(Code::Unschedulable.is_resolvable_by_preemption());
        assert!(!Code::UnschedulableAndUnresolvable.is_resolvable_by_preemption());
        assert!(!Code::Error.is_resolvable_by_preemption());
        assert!(!Code::Success.is_resolvable_by_preemption());
    }

    #[test]
    fn a_node_rejected_as_unresolvable_is_not_a_preemption_candidate() {
        let mut n = NodeToStatus::default();
        n.record("fits-nothing", Status::unschedulable("NodeResourcesFit", "Insufficient cpu"));
        n.record("wrong-name", Status::unresolvable("NodeName", "node(s) didn't match"));

        assert_eq!(n.preemption_candidates(), vec!["fits-nothing"]);
    }

    #[test]
    fn every_rejecting_plugin_is_recorded_exactly_once() {
        // The requeue path keys off this set; a plugin missing from it can
        // never be woken, and a duplicate would run its hint twice.
        let mut n = NodeToStatus::default();
        n.record("a", Status::unschedulable("NodeResourcesFit", "Insufficient cpu"));
        n.record("b", Status::unschedulable("NodeResourcesFit", "Insufficient memory"));
        n.record("c", Status::unresolvable("TaintToleration", "untolerated taint"));

        assert_eq!(n.rejecting_plugins(), vec!["NodeResourcesFit", "TaintToleration"]);
    }

    #[test]
    fn pending_is_a_rejection_but_a_forgiving_one() {
        // It rejects the pod, but the queue must not charge it backoff.
        assert!(Code::Pending.is_rejection());
        assert!(!Code::Pending.is_resolvable_by_preemption());
    }

    #[test]
    fn the_summary_reads_like_kubectl_describe() {
        let mut n = NodeToStatus::default();
        n.record("a", Status::unschedulable("NodeResourcesFit", "Insufficient cpu"));
        n.record("b", Status::unschedulable("NodeResourcesFit", "Insufficient cpu"));
        n.record("c", Status::unresolvable("TaintToleration", "node(s) had untolerated taint"));

        assert_eq!(
            n.summary(3),
            "0/3 nodes are available: 2 Insufficient cpu, 1 node(s) had untolerated taint"
        );
    }
}
