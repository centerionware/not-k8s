//! Deciding whether a cluster change is worth retrying a pod for.
//!
//! # The rule that makes this subsystem dangerous
//!
//! **A pod rejected by plugin P will only ever be retried by an event P
//! registered.** If a plugin can reject and forgets to subscribe to the change
//! that would unblock it, the pod does not error, does not log, and does not
//! fail any test — it simply sits Pending until the five-minute safety net
//! sweeps it up. On a cluster that is otherwise working, that reads as "the
//! scheduler is slow sometimes", and it is the single largest source of bugs
//! in upstream's equivalent.
//!
//! Two things guard against it here: every plugin asserts its exact event set
//! in its own unit tests, and the safety net logs loudly whenever it actually
//! rescues something (see `mod.rs`), so an incomplete set announces itself in
//! production instead of hiding.
//!
//! # Failing open
//!
//! Where a hint cannot decide, the answer is [`QueueingHint::Queue`]. Retrying
//! a pod that had no chance costs one wasted cycle; failing to retry one that
//! did costs five minutes of a workload not running. The asymmetry is not
//! close, so every ambiguous path in this file resolves towards Queue.

use crate::cache::PodInfo;
use crate::events::ClusterEvent;
use crate::framework::{ChangedObject, ClusterEventWithHint, QueueingHint};
use std::collections::HashMap;

/// Where a pod goes when an event says it is worth another look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequeueDecision {
    /// Nothing this pod was rejected for has changed. Leave it where it is.
    Skip,
    /// Retry it, but make it serve its backoff first — the last cycle it ran
    /// was wasted work.
    AfterBackoff,
    /// Retry it immediately, skipping backoff entirely. Reserved for
    /// [`crate::framework::status::Code::Pending`] rejections, which mean
    /// "I rejected you but real progress was made" — charging backoff there
    /// penalises a pod for someone else's success.
    Immediately,
}

/// Which events each plugin subscribed to, built once from the enabled set.
#[derive(Default)]
pub struct HintRegistry {
    by_plugin: HashMap<&'static str, Vec<ClusterEventWithHint>>,
}

impl HintRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: &'static str, events: Vec<ClusterEventWithHint>) {
        // Extend rather than insert: a plugin implementing several extension
        // points is registered once per point, and replacing would silently
        // keep only the last one's subscriptions.
        self.by_plugin.entry(plugin).or_default().extend(events);
    }

    /// Whether this plugin, having rejected this pod, wants it retried.
    fn plugin_wants_requeue(
        &self,
        plugin: &str,
        pod: &PodInfo,
        event: &ClusterEvent,
        old: Option<&ChangedObject>,
        new: Option<&ChangedObject>,
    ) -> bool {
        let Some(subscriptions) = self.by_plugin.get(plugin) else {
            // A plugin that rejected a pod but registered nothing. Legitimate
            // for NodeName (nothing can ever help), so this is not an error —
            // but it does mean no event will ever wake the pod on its account.
            return false;
        };
        subscriptions.iter().any(|sub| {
            if !sub.event.matches(event) {
                return false;
            }
            match &sub.hint {
                None => true,
                Some(f) => f.hint(pod, old, new) == QueueingHint::Queue,
            }
        })
    }

    /// The routing decision for one pod against one event.
    ///
    /// `pending` is checked first and wins outright: when a pod was rejected
    /// by both a Pending plugin and an ordinary one, the Pending verdict is
    /// the more informative — something genuinely advanced — and making it
    /// wait out a backoff it did not earn is the behaviour that motivated the
    /// distinction existing at all.
    pub fn decide(
        &self,
        pod: &PodInfo,
        unschedulable_plugins: &[&'static str],
        pending_plugins: &[&'static str],
        event: &ClusterEvent,
        old: Option<&ChangedObject>,
        new: Option<&ChangedObject>,
    ) -> RequeueDecision {
        if pending_plugins
            .iter()
            .any(|p| self.plugin_wants_requeue(p, pod, event, old, new))
        {
            return RequeueDecision::Immediately;
        }
        if unschedulable_plugins
            .iter()
            .any(|p| self.plugin_wants_requeue(p, pod, event, old, new))
        {
            return RequeueDecision::AfterBackoff;
        }
        RequeueDecision::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ActionType, EventResource};
    use crate::framework::QueueingHintFn;

    struct AlwaysSkip;
    impl QueueingHintFn for AlwaysSkip {
        fn hint(
            &self,
            _pod: &PodInfo,
            _old: Option<&ChangedObject>,
            _new: Option<&ChangedObject>,
        ) -> QueueingHint {
            QueueingHint::Skip
        }
    }

    struct AlwaysQueue;
    impl QueueingHintFn for AlwaysQueue {
        fn hint(
            &self,
            _pod: &PodInfo,
            _old: Option<&ChangedObject>,
            _new: Option<&ChangedObject>,
        ) -> QueueingHint {
            QueueingHint::Queue
        }
    }

    fn node_added() -> ClusterEvent {
        ClusterEvent::new(EventResource::Node, ActionType::ADD)
    }

    fn registry_with(
        plugin: &'static str,
        event: ClusterEvent,
        hint: Option<Box<dyn QueueingHintFn>>,
    ) -> HintRegistry {
        let mut r = HintRegistry::new();
        r.register(plugin, vec![ClusterEventWithHint { event, hint }]);
        r
    }

    #[test]
    fn a_matching_event_sends_the_pod_to_backoff() {
        let r = registry_with("Fit", node_added(), None);
        assert_eq!(
            r.decide(&PodInfo::default(), &["Fit"], &[], &node_added(), None, None),
            RequeueDecision::AfterBackoff
        );
    }

    #[test]
    fn an_unrelated_event_leaves_the_pod_alone() {
        let r = registry_with(
            "Fit",
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_ALLOCATABLE),
            None,
        );
        let label_change =
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_LABEL);

        assert_eq!(
            r.decide(&PodInfo::default(), &["Fit"], &[], &label_change, None, None),
            RequeueDecision::Skip
        );
    }

    #[test]
    fn a_pending_rejection_skips_backoff_entirely() {
        // The pod did not waste the cycle — something genuinely advanced.
        let r = registry_with("DynamicResources", node_added(), None);
        assert_eq!(
            r.decide(&PodInfo::default(), &[], &["DynamicResources"], &node_added(), None, None),
            RequeueDecision::Immediately
        );
    }

    #[test]
    fn pending_wins_when_a_pod_was_rejected_by_both_kinds() {
        let mut r = HintRegistry::new();
        r.register("Fit", vec![ClusterEventWithHint::always(node_added())]);
        r.register("DynamicResources", vec![ClusterEventWithHint::always(node_added())]);

        assert_eq!(
            r.decide(
                &PodInfo::default(),
                &["Fit"],
                &["DynamicResources"],
                &node_added(),
                None,
                None
            ),
            RequeueDecision::Immediately
        );
    }

    #[test]
    fn a_hint_can_veto_an_otherwise_matching_event() {
        // The narrowing that makes this worth having: the event type matches,
        // but this particular pod could not have been helped.
        let r = registry_with("Fit", node_added(), Some(Box::new(AlwaysSkip)));
        assert_eq!(
            r.decide(&PodInfo::default(), &["Fit"], &[], &node_added(), None, None),
            RequeueDecision::Skip
        );
    }

    #[test]
    fn a_hint_that_says_queue_is_honoured() {
        let r = registry_with("Fit", node_added(), Some(Box::new(AlwaysQueue)));
        assert_eq!(
            r.decide(&PodInfo::default(), &["Fit"], &[], &node_added(), None, None),
            RequeueDecision::AfterBackoff
        );
    }

    #[test]
    fn a_missing_hint_means_always_requeue() {
        // Failing open: cheaper to waste a cycle than to strand a workload.
        let r = registry_with("Fit", node_added(), None);
        assert_eq!(
            r.decide(&PodInfo::default(), &["Fit"], &[], &node_added(), None, None),
            RequeueDecision::AfterBackoff
        );
    }

    #[test]
    fn a_plugin_that_registered_nothing_never_requeues_its_rejections() {
        // Legitimate for NodeName — spec.nodeName is immutable, so nothing
        // can help — and a silent stall for anyone else, which is why every
        // plugin asserts its own event set.
        let r = HintRegistry::new();
        assert_eq!(
            r.decide(&PodInfo::default(), &["NodeName"], &[], &node_added(), None, None),
            RequeueDecision::Skip
        );
    }

    #[test]
    fn only_the_plugins_that_rejected_this_pod_are_consulted() {
        // A plugin that admitted the pod has no say in whether it is retried.
        let r = registry_with("Fit", node_added(), None);
        assert_eq!(
            r.decide(&PodInfo::default(), &["TaintToleration"], &[], &node_added(), None, None),
            RequeueDecision::Skip
        );
    }

    #[test]
    fn registering_a_plugin_twice_keeps_both_subscriptions() {
        // A plugin implementing several extension points registers once per
        // point; replacing would silently drop all but the last.
        let mut r = HintRegistry::new();
        let taint_change =
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_TAINT);
        r.register("Multi", vec![ClusterEventWithHint::always(node_added())]);
        r.register("Multi", vec![ClusterEventWithHint::always(taint_change)]);

        assert_eq!(
            r.decide(&PodInfo::default(), &["Multi"], &[], &node_added(), None, None),
            RequeueDecision::AfterBackoff
        );
        assert_eq!(
            r.decide(&PodInfo::default(), &["Multi"], &[], &taint_change, None, None),
            RequeueDecision::AfterBackoff
        );
    }

    #[test]
    fn the_timeout_safety_net_reaches_every_rejected_pod() {
        // Whatever a pod was rejected for, the net must be able to rescue it —
        // that is the entire reason it exists.
        let r = registry_with(
            "Fit",
            ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_ALLOCATABLE),
            None,
        );
        assert_eq!(
            r.decide(
                &PodInfo::default(),
                &["Fit"],
                &[],
                &crate::events::UNSCHEDULABLE_TIMEOUT,
                None,
                None
            ),
            RequeueDecision::AfterBackoff
        );
    }
}
