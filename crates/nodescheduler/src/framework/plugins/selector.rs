//! `LabelSelector` matching, shared by every plugin that asks "which pods".
//!
//! Both topology plugins are built on the same question — given a selector,
//! which of the pods already on this node (or in this topology domain) match
//! it — and getting that question wrong is invisible: a selector that matches
//! too little silently disables an anti-affinity rule, and one that matches
//! too much silently blocks scheduling. So it lives here once, tested
//! directly, rather than being reimplemented per plugin.
//!
//! # Empty is not absent, again
//!
//! The same trap as node affinity, in a different shape. An **empty**
//! `LabelSelector` (`{}`) matches **every** pod; an **absent** one (`None`)
//! matches **none**. That is the opposite of the intuition for a filter, and
//! it is load-bearing: `podAffinityTerm.labelSelector: {}` means "any pod",
//! which is how "spread me away from everything" is written.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use std::collections::BTreeMap;

/// Whether a set of labels satisfies a selector.
///
/// `None` matches nothing — see the module header.
pub fn matches_selector(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool {
    let Some(selector) = selector else {
        return false;
    };

    // matchLabels: every entry must be present and equal.
    for (k, v) in selector.match_labels.iter().flatten() {
        if labels.get(k) != Some(v) {
            return false;
        }
    }

    // matchExpressions: every requirement must hold.
    for req in selector.match_expressions.iter().flatten() {
        let actual = labels.get(&req.key);
        let values = req.values.clone().unwrap_or_default();
        let ok = match req.operator.as_str() {
            "In" => actual.is_some_and(|a| values.contains(a)),
            "NotIn" => actual.is_none_or(|a| !values.contains(a)),
            "Exists" => actual.is_some(),
            "DoesNotExist" => actual.is_none(),
            // The apiserver validates this enum, so anything else is a
            // malformed object. Not matching is the safe reading — the same
            // choice node_affinity.rs makes, for the same reason.
            _ => false,
        };
        if !ok {
            return false;
        }
    }

    true
}

/// The namespaces a `podAffinityTerm` applies to.
///
/// Upstream's rule, which is easy to get subtly wrong: the explicit
/// `namespaces` list and the `namespaceSelector` are **unioned**, and if both
/// are absent the term means the pod's *own* namespace. An empty
/// `namespaceSelector` (`{}`) means every namespace.
///
/// A `namespaceSelector` with real requirements is carried as
/// [`NamespaceScope::Selected`] and resolved against the namespace labels
/// the scheduler's own Namespace watch supplies — see `namespace_in_scope`.
#[derive(Debug, PartialEq)]
pub enum NamespaceScope {
    /// Only the incoming pod's own namespace.
    OwnOnly,
    /// This explicit set.
    Explicit(Vec<String>),
    /// Every namespace (`namespaceSelector: {}`).
    All,
    /// A `namespaceSelector` with real requirements, resolved against the
    /// namespace labels the scheduler watches.
    Selected(LabelSelector),
}

pub fn namespace_scope(
    namespaces: Option<&Vec<String>>,
    namespace_selector: Option<&LabelSelector>,
) -> NamespaceScope {
    let explicit: Vec<String> = namespaces.cloned().unwrap_or_default();

    match namespace_selector {
        None => {
            if explicit.is_empty() {
                NamespaceScope::OwnOnly
            } else {
                NamespaceScope::Explicit(explicit)
            }
        }
        Some(sel) => {
            let empty = sel.match_labels.as_ref().is_none_or(|m| m.is_empty())
                && sel.match_expressions.as_ref().is_none_or(|m| m.is_empty());
            if empty {
                NamespaceScope::All
            } else {
                NamespaceScope::Selected(sel.clone())
            }
        }
    }
}

/// Whether a pod in `pod_namespace` is inside `scope`, for a term declared by
/// a pod in `own_namespace`.
///
/// `namespace_labels` maps namespace name to its labels — the scheduler's
/// Namespace watch. An earlier version had no such watch and made
/// `namespaceSelector` terms match everything, on the reasoning that
/// over-matching only refuses a placement while under-matching silently
/// disables a rule. That was the better of two wrong answers, and it is not
/// what upstream does: the selector is evaluated against the real labels.
pub fn namespace_in_scope(
    scope: &NamespaceScope,
    own_namespace: &str,
    pod_namespace: &str,
    namespace_labels: &std::collections::HashMap<String, BTreeMap<String, String>>,
) -> bool {
    match scope {
        NamespaceScope::OwnOnly => pod_namespace == own_namespace,
        NamespaceScope::Explicit(list) => list.iter().any(|n| n == pod_namespace),
        NamespaceScope::All => true,
        NamespaceScope::Selected(sel) => namespace_labels
            .get(pod_namespace)
            .is_some_and(|labels| matches_selector(Some(sel), labels)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn sel_labels(pairs: &[(&str, &str)]) -> LabelSelector {
        LabelSelector {
            match_labels: Some(labels(pairs)),
            match_expressions: None,
        }
    }

    #[test]
    fn an_absent_selector_matches_nothing() {
        assert!(!matches_selector(None, &labels(&[("app", "web")])));
    }

    #[test]
    fn an_empty_selector_matches_everything() {
        // How "spread me away from every pod" is written. Reading this as
        // "matches nothing" silently disables the rule.
        let empty = LabelSelector { match_labels: None, match_expressions: None };
        assert!(matches_selector(Some(&empty), &labels(&[("app", "web")])));
        assert!(matches_selector(Some(&empty), &BTreeMap::new()));
    }

    #[test]
    fn match_labels_requires_every_entry() {
        let sel = sel_labels(&[("app", "web"), ("tier", "front")]);
        assert!(matches_selector(Some(&sel), &labels(&[("app", "web"), ("tier", "front")])));
        assert!(!matches_selector(Some(&sel), &labels(&[("app", "web")])));
    }

    #[test]
    fn every_match_expression_operator_behaves() {
        let l = labels(&[("app", "web"), ("tier", "front")]);
        let expr = |key: &str, op: &str, vals: &[&str]| LabelSelector {
            match_labels: None,
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: key.to_string(),
                operator: op.to_string(),
                values: Some(vals.iter().map(|s| s.to_string()).collect()),
            }]),
        };

        assert!(matches_selector(Some(&expr("app", "In", &["web", "api"])), &l));
        assert!(!matches_selector(Some(&expr("app", "In", &["api"])), &l));
        assert!(matches_selector(Some(&expr("app", "NotIn", &["api"])), &l));
        assert!(matches_selector(Some(&expr("app", "Exists", &[])), &l));
        assert!(!matches_selector(Some(&expr("nope", "Exists", &[])), &l));
        assert!(matches_selector(Some(&expr("nope", "DoesNotExist", &[])), &l));
        assert!(!matches_selector(Some(&expr("app", "DoesNotExist", &[])), &l));
    }

    #[test]
    fn notin_is_satisfied_by_a_label_that_is_absent() {
        let l = BTreeMap::new();
        let sel = LabelSelector {
            match_labels: None,
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "app".to_string(),
                operator: "NotIn".to_string(),
                values: Some(vec!["web".to_string()]),
            }]),
        };
        assert!(matches_selector(Some(&sel), &l));
    }

    #[test]
    fn an_unknown_operator_matches_nothing_rather_than_everything() {
        let sel = LabelSelector {
            match_labels: None,
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "app".to_string(),
                operator: "Sortof".to_string(),
                values: None,
            }]),
        };
        assert!(!matches_selector(Some(&sel), &labels(&[("app", "web")])));
    }

    // ── Namespace scope ─────────────────────────────────────────────────

    #[test]
    fn no_namespaces_and_no_selector_means_the_pods_own_namespace() {
        assert_eq!(namespace_scope(None, None), NamespaceScope::OwnOnly);
        let ns = std::collections::HashMap::new();
        assert!(namespace_in_scope(&NamespaceScope::OwnOnly, "prod", "prod", &ns));
        assert!(!namespace_in_scope(&NamespaceScope::OwnOnly, "prod", "dev", &ns));
    }

    #[test]
    fn an_explicit_list_is_used_verbatim() {
        let scope = namespace_scope(Some(&vec!["a".to_string(), "b".to_string()]), None);
        assert_eq!(scope, NamespaceScope::Explicit(vec!["a".to_string(), "b".to_string()]));
        let ns = std::collections::HashMap::new();
        assert!(namespace_in_scope(&scope, "own", "a", &ns));
        assert!(!namespace_in_scope(&scope, "own", "c", &ns));
        assert!(
            !namespace_in_scope(&scope, "own", "own", &ns),
            "an explicit list replaces the own-namespace default, it does not extend it"
        );
    }

    #[test]
    fn an_empty_namespace_selector_means_every_namespace() {
        let empty = LabelSelector { match_labels: None, match_expressions: None };
        let scope = namespace_scope(None, Some(&empty));
        assert_eq!(scope, NamespaceScope::All);
        let ns = std::collections::HashMap::new();
        assert!(namespace_in_scope(&scope, "prod", "anything", &ns));
    }

    #[test]
    fn a_namespace_selector_is_evaluated_against_real_namespace_labels() {
        // Not approximated in either direction. An earlier version matched
        // everything for want of a Namespace watch; that was the better of
        // two wrong answers rather than the right one.
        let scope = namespace_scope(None, Some(&sel_labels(&[("env", "prod")])));
        let mut ns = std::collections::HashMap::new();
        ns.insert("prod-a".to_string(), labels(&[("env", "prod")]));
        ns.insert("staging".to_string(), labels(&[("env", "staging")]));

        assert!(namespace_in_scope(&scope, "own", "prod-a", &ns));
        assert!(!namespace_in_scope(&scope, "own", "staging", &ns));
        assert!(
            !namespace_in_scope(&scope, "own", "never-heard-of-it", &ns),
            "a namespace with no known labels cannot satisfy a selector"
        );
    }
}
