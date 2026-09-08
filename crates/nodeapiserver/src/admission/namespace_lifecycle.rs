//! `NamespaceLifecycle` — a faithful port of real upstream's built-in
//! admission plugin
//! (`staging/src/k8s.io/apiserver/pkg/admission/plugin/namespace/lifecycle/admission.go`,
//! release-1.34, fetched and read directly): enforces the three real
//! life-cycle rules around a `Namespace`'s existence/phase that every real
//! Kubernetes cluster applies unconditionally (this plugin isn't in any
//! operator-toggleable list upstream either — it's part of the default
//! admission chain, not opt-in), so this crate wires it in the same way:
//! **on, unconditionally**, no config flag (unlike Group I's RBAC, which
//! needs an operator's own bootstrap `ClusterRole`/`ClusterRoleBinding` set
//! to exist first — this plugin needs no bootstrap data, so there's no
//! equivalent "could lock every request out" risk to gate against).
//!
//! Real rules, ported:
//! 1. Deleting one of the three immortal namespaces (`default`,
//!    `kube-system`, `kube-public` — upstream's own literal
//!    `NewLifecycle(sets.NewString(metav1.NamespaceDefault,
//!    metav1.NamespaceSystem, metav1.NamespacePublic))` registration args,
//!    not a guess) is always forbidden.
//! 2. `Namespace` objects themselves, non-namespaced resources, and
//!    `DELETE` of any other resource are always allowed by this plugin
//!    (upstream's own `isAccessReview` early-out for
//!    `authorization.k8s.io/localsubjectaccessreviews` is ported too, same
//!    reasoning: leaking "does this namespace exist" via a 404 on an access
//!    review would defeat its own purpose).
//! 3. Everything else (`CREATE`/`UPDATE` of a namespaced resource) requires
//!    the target namespace to actually exist, and forbids `CREATE` into a
//!    namespace whose `status.phase` is `Terminating`.
//!
//! **Named honestly, not silently simplified**: upstream's version also
//! carries an in-process `namespaceLister` (informer cache) with a 50ms
//! "give the cache time to observe a just-created namespace" wait and an
//! `LRUExpireCache`-backed `forceLiveLookup` list to route around cache
//! staleness after a namespace `DELETE`. This crate has no such cache in
//! the admission path at all — [`decide`] always resolves the namespace
//! straight from storage (`server::rest::get`, the same generic path every
//! other read in this crate already takes) — so every one of those
//! staleness workarounds is genuinely inapplicable, not dropped for
//! expedience: there is no cache here to be stale.
//!
//! Split into a pure decision (`quick_decision`/`decide`, this file, unit
//! tested with no I/O) and the one real I/O step a caller performs in
//! between (`server::listener::handle` calls `server::rest::get` for the
//! namespace, only when [`QuickDecision::NeedsNamespaceLookup`] says to) —
//! the same pure/orchestration split `cacher::driver` already established.

use crate::admission::attributes::{Attributes, Operation};

const IMMORTAL_NAMESPACES: [&str; 3] = ["default", "kube-system", "kube-public"];

/// Upstream's own `accessReviewResources` map — genuinely just this one
/// entry in real upstream too, not a subset picked by this crate.
const ACCESS_REVIEW_GROUP: &str = "authorization.k8s.io";
const ACCESS_REVIEW_RESOURCE: &str = "localsubjectaccessreviews";

fn is_namespace(attrs: &Attributes<'_>) -> bool {
    attrs.group.is_empty() && attrs.resource == "namespaces"
}

fn is_access_review(attrs: &Attributes<'_>) -> bool {
    attrs.group == ACCESS_REVIEW_GROUP && attrs.resource == ACCESS_REVIEW_RESOURCE
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow,
    /// Real upstream's own `errors.NewForbidden` — surfaces as a `403`.
    Forbidden(String),
    /// The one case where this plugin denies with the *real* underlying
    /// error rather than wrapping it as `Forbidden` — real upstream
    /// returns the live lookup's own `NotFound` error unwrapped (`case
    /// errors.IsNotFound(err): return err`), so a `CREATE`/`UPDATE` into a
    /// namespace that plain doesn't exist surfaces as a genuine `404`, not
    /// a `403`.
    NamespaceNotFound(String),
}

#[derive(Debug, PartialEq)]
pub enum QuickDecision {
    Allow,
    Forbidden(String),
    /// This plugin can't decide without knowing the target namespace's
    /// current existence/phase — the caller must fetch it (`server::rest::get`
    /// on `("", "namespaces", None, attrs.namespace)`) and call [`decide`]
    /// with the result.
    NeedsNamespaceLookup,
}

/// The parts decidable with no I/O — mirrors real upstream's own early
/// returns, in the same order, before it ever touches its namespace
/// lister.
pub fn quick_decision(attrs: &Attributes<'_>) -> QuickDecision {
    if attrs.operation == Operation::Delete && is_namespace(attrs) && IMMORTAL_NAMESPACES.contains(&attrs.name) {
        return QuickDecision::Forbidden(format!("namespace {:?} may not be deleted", attrs.name));
    }

    // Always allow non-namespaced resources (upstream: `len(a.GetNamespace()) == 0 && ... != Namespace`).
    if attrs.namespace.is_empty() && !is_namespace(attrs) {
        return QuickDecision::Allow;
    }

    // Always allow all operations to Namespace objects themselves.
    if is_namespace(attrs) {
        return QuickDecision::Allow;
    }

    // Always allow deletion of any other resource.
    if attrs.operation == Operation::Delete {
        return QuickDecision::Allow;
    }

    // Always allow access-review checks — returning a 404 here would leak
    // whether the namespace exists, defeating their purpose.
    if is_access_review(attrs) {
        return QuickDecision::Allow;
    }

    QuickDecision::NeedsNamespaceLookup
}

/// Called only after [`quick_decision`] returned [`QuickDecision::NeedsNamespaceLookup`].
/// `namespace_phase` is `None` if the namespace doesn't exist at all
/// (`server::rest::GetOutcome::ObjectNotFound`), `Some(phase)` — the real
/// `status.phase` string (`"Active"`/`"Terminating"`, or `""` for a
/// namespace with no status set yet) — otherwise.
pub fn decide(attrs: &Attributes<'_>, namespace_phase: Option<&str>) -> Decision {
    let Some(phase) = namespace_phase else {
        return Decision::NamespaceNotFound(format!("namespace {:?} not found", attrs.namespace));
    };

    if attrs.operation == Operation::Create && phase == "Terminating" {
        return Decision::Forbidden(format!("unable to create new content in namespace {:?} because it is being terminated", attrs.namespace));
    }

    Decision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs<'a>(operation: Operation, group: &'a str, resource: &'a str, namespace: &'a str, name: &'a str) -> Attributes<'a> {
        Attributes { operation, group, resource, namespace, name }
    }

    #[test]
    fn deleting_an_immortal_namespace_is_forbidden() {
        for name in IMMORTAL_NAMESPACES {
            let a = attrs(Operation::Delete, "", "namespaces", "", name);
            assert!(matches!(quick_decision(&a), QuickDecision::Forbidden(_)), "{name} must be immortal");
        }
    }

    #[test]
    fn deleting_a_non_immortal_namespace_needs_no_lookup_and_is_allowed() {
        let a = attrs(Operation::Delete, "", "namespaces", "", "my-app");
        assert_eq!(quick_decision(&a), QuickDecision::Allow);
    }

    #[test]
    fn a_cluster_scoped_resource_is_always_allowed_with_no_lookup() {
        let a = attrs(Operation::Create, "", "nodes", "", "worker-1");
        assert_eq!(quick_decision(&a), QuickDecision::Allow);
    }

    #[test]
    fn deleting_a_namespaced_resource_is_always_allowed_with_no_lookup() {
        let a = attrs(Operation::Delete, "", "pods", "default", "web-1");
        assert_eq!(quick_decision(&a), QuickDecision::Allow);
    }

    #[test]
    fn an_access_review_check_is_always_allowed_with_no_lookup() {
        let a = attrs(Operation::Create, "authorization.k8s.io", "localsubjectaccessreviews", "some-ns", "");
        assert_eq!(quick_decision(&a), QuickDecision::Allow);
    }

    #[test]
    fn a_different_authorization_resource_is_not_treated_as_an_access_review() {
        let a = attrs(Operation::Create, "authorization.k8s.io", "subjectaccessreviews", "some-ns", "");
        assert_eq!(quick_decision(&a), QuickDecision::NeedsNamespaceLookup);
    }

    #[test]
    fn creating_into_a_namespaced_resource_needs_a_namespace_lookup() {
        let a = attrs(Operation::Create, "", "pods", "default", "web-1");
        assert_eq!(quick_decision(&a), QuickDecision::NeedsNamespaceLookup);
    }

    #[test]
    fn updating_into_a_namespaced_resource_needs_a_namespace_lookup() {
        let a = attrs(Operation::Update, "apps", "deployments", "default", "web");
        assert_eq!(quick_decision(&a), QuickDecision::NeedsNamespaceLookup);
    }

    #[test]
    fn create_into_a_missing_namespace_is_not_found_not_forbidden() {
        let a = attrs(Operation::Create, "", "pods", "ghost-ns", "web-1");
        assert_eq!(decide(&a, None), Decision::NamespaceNotFound("namespace \"ghost-ns\" not found".to_string()));
    }

    #[test]
    fn create_into_a_terminating_namespace_is_forbidden() {
        let a = attrs(Operation::Create, "", "pods", "dying-ns", "web-1");
        assert!(matches!(decide(&a, Some("Terminating")), Decision::Forbidden(_)));
    }

    #[test]
    fn create_into_an_active_namespace_is_allowed() {
        let a = attrs(Operation::Create, "", "pods", "default", "web-1");
        assert_eq!(decide(&a, Some("Active")), Decision::Allow);
    }

    #[test]
    fn update_into_a_terminating_namespace_is_still_allowed() {
        // Real upstream only blocks CREATE on a terminating namespace, not
        // UPDATE (an in-flight resource must still be updatable — e.g. to
        // remove its own finalizers — while its namespace finishes tearing
        // down).
        let a = attrs(Operation::Update, "", "pods", "dying-ns", "web-1");
        assert_eq!(decide(&a, Some("Terminating")), Decision::Allow);
    }

    #[test]
    fn a_namespace_with_no_status_phase_set_yet_is_treated_as_not_terminating() {
        let a = attrs(Operation::Create, "", "pods", "new-ns", "web-1");
        assert_eq!(decide(&a, Some("")), Decision::Allow);
    }
}
