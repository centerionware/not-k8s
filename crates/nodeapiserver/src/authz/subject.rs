//! Subject matching: does a `RoleBinding`/`ClusterRoleBinding`'s
//! `Subjects` list include a given authenticated user? A faithful port of
//! real upstream's own `pkg/registry/rbac/validation/rule.go`'s
//! `appliesTo`/`appliesToUser`, plus the `ServiceAccount` username
//! convention (`staging/src/k8s.io/apiserver/pkg/authentication/serviceaccount/util.go`'s
//! `MakeUsername`/`MatchesUsername`) — all fetched and read directly, not
//! reconstructed from memory.
//!
//! # What this is, and isn't yet
//!
//! Pure subject-matching only — same "land the primitive, wire it later"
//! split `rbac.rs`'s own module doc comment already applies to rule
//! matching. Nothing here fetches a real `RoleBinding`/`ClusterRoleBinding`
//! from storage; a caller with an already-decoded `Subject` list (however
//! it got them) can use this to find which one, if any, applies to a
//! given user.

/// `pkg/apis/rbac/v1/types.go`'s `Subject`. `namespace`/`api_group` are
/// only meaningful for `Kind::ServiceAccount` (namespace) — real upstream
/// keeps `APIGroup` on every subject for schema uniformity but RBAC's own
/// matching logic (`appliesToUser`) never reads it, so it's not
/// represented here at all; adding a field nothing consults would be
/// exactly the kind of dead data this crate's own `codec` module doc
/// comments warn against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub name: String,
    /// Only set for `Kind::ServiceAccount` — real upstream's own `Subject.Namespace`
    /// doc comment: "This field is only applicable for actual users of
    /// type ServiceAccount."
    pub namespace: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    User,
    Group,
    ServiceAccount,
}

/// Real upstream's own `ServiceAccountUsernamePrefix`/`Separator` — the
/// `system:serviceaccount:<namespace>:<name>` convention every
/// ServiceAccount token's `user.Info.Name` follows.
const SERVICE_ACCOUNT_USERNAME_PREFIX: &str = "system:serviceaccount:";
const SERVICE_ACCOUNT_USERNAME_SEPARATOR: &str = ":";

/// `MakeUsername`: the canonical username for a ServiceAccount identity.
pub fn service_account_username(namespace: &str, name: &str) -> String {
    format!("{SERVICE_ACCOUNT_USERNAME_PREFIX}{namespace}{SERVICE_ACCOUNT_USERNAME_SEPARATOR}{name}")
}

/// `MatchesUsername`: a faithful port of upstream's own allocation-free
/// prefix-stripping check — kept as the same sequence of strip-or-fail
/// steps (not simplified to `username == service_account_username(...)`)
/// so it stays obviously equivalent to the real algorithm even though the
/// simpler form would behave identically in Rust.
pub fn matches_service_account_username(namespace: &str, name: &str, username: &str) -> bool {
    let Some(rest) = username.strip_prefix(SERVICE_ACCOUNT_USERNAME_PREFIX) else { return false };
    let Some(rest) = rest.strip_prefix(namespace) else { return false };
    let Some(rest) = rest.strip_prefix(SERVICE_ACCOUNT_USERNAME_SEPARATOR) else { return false };
    rest == name
}

/// `appliesToUser`: does `subject` (from a binding in `binding_namespace`
/// — `""` for a `ClusterRoleBinding`) apply to a user with `user_name`/
/// `user_groups`?
pub fn subject_matches(user_name: &str, user_groups: &[String], subject: &Subject, binding_namespace: &str) -> bool {
    match subject.kind {
        SubjectKind::User => user_name == subject.name,
        SubjectKind::Group => user_groups.iter().any(|g| g == &subject.name),
        SubjectKind::ServiceAccount => {
            // Real upstream: an unqualified Subject.Namespace defaults to
            // the binding's own namespace (lets a RoleBinding reference a
            // same-namespace ServiceAccount without repeating it) — a
            // ClusterRoleBinding has no namespace of its own
            // (binding_namespace == ""), so a ServiceAccount subject on
            // one MUST name its namespace explicitly or this can never
            // match anything.
            let sa_namespace = if subject.namespace.is_empty() { binding_namespace } else { subject.namespace.as_str() };
            if sa_namespace.is_empty() {
                return false;
            }
            matches_service_account_username(sa_namespace, &subject.name, user_name)
        }
    }
}

/// `appliesTo`: the index of the first subject in `subjects` that applies
/// to this user, if any — real upstream returns this (not just a bool)
/// because callers use it to describe *which* subject granted access in
/// audit/log messages; kept for the same reason here even though no
/// caller consumes it yet.
pub fn first_applicable_subject(user_name: &str, user_groups: &[String], subjects: &[Subject], binding_namespace: &str) -> Option<usize> {
    subjects.iter().position(|s| subject_matches(user_name, user_groups, s, binding_namespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_subject(name: &str) -> Subject {
        Subject { kind: SubjectKind::User, name: name.to_string(), namespace: String::new() }
    }
    fn group_subject(name: &str) -> Subject {
        Subject { kind: SubjectKind::Group, name: name.to_string(), namespace: String::new() }
    }
    fn sa_subject(name: &str, namespace: &str) -> Subject {
        Subject { kind: SubjectKind::ServiceAccount, name: name.to_string(), namespace: namespace.to_string() }
    }

    #[test]
    fn a_user_subject_matches_by_exact_name() {
        let s = user_subject("alice");
        assert!(subject_matches("alice", &[], &s, ""));
        assert!(!subject_matches("bob", &[], &s, ""));
    }

    #[test]
    fn a_group_subject_matches_if_the_group_is_in_the_users_groups() {
        let s = group_subject("system:masters");
        let groups = vec!["system:authenticated".to_string(), "system:masters".to_string()];
        assert!(subject_matches("alice", &groups, &s, ""));
        assert!(!subject_matches("alice", &["system:authenticated".to_string()], &s, ""));
    }

    #[test]
    fn service_account_username_round_trips_through_matches() {
        let username = service_account_username("kube-system", "coredns");
        assert_eq!(username, "system:serviceaccount:kube-system:coredns");
        assert!(matches_service_account_username("kube-system", "coredns", &username));
        assert!(!matches_service_account_username("default", "coredns", &username));
        assert!(!matches_service_account_username("kube-system", "other-sa", &username));
    }

    #[test]
    fn matches_service_account_username_rejects_a_non_service_account_username() {
        assert!(!matches_service_account_username("default", "web", "alice"));
    }

    #[test]
    fn a_service_account_subject_defaults_its_namespace_to_the_bindings_own() {
        // A RoleBinding in "default" naming an SA subject with no
        // namespace of its own must resolve against "default".
        let s = sa_subject("web-sa", "");
        let username = service_account_username("default", "web-sa");
        assert!(subject_matches(&username, &[], &s, "default"));
        assert!(!subject_matches(&username, &[], &s, "other-namespace"));
    }

    #[test]
    fn a_service_account_subject_with_an_explicit_namespace_overrides_the_bindings() {
        let s = sa_subject("web-sa", "explicit-ns");
        let username = service_account_username("explicit-ns", "web-sa");
        // Even though the binding lives in "default", the subject's own
        // explicit namespace wins.
        assert!(subject_matches(&username, &[], &s, "default"));
    }

    #[test]
    fn a_cluster_role_binding_service_account_subject_needs_an_explicit_namespace() {
        // binding_namespace == "" (a ClusterRoleBinding) with no explicit
        // subject namespace can never match anything real.
        let s = sa_subject("web-sa", "");
        let username = service_account_username("default", "web-sa");
        assert!(!subject_matches(&username, &[], &s, ""));
    }

    #[test]
    fn first_applicable_subject_finds_the_first_match_by_index() {
        let subjects = vec![user_subject("bob"), group_subject("system:masters"), user_subject("alice")];
        let groups = vec!["system:masters".to_string()];
        assert_eq!(first_applicable_subject("alice", &groups, &subjects, ""), Some(1));
    }

    #[test]
    fn first_applicable_subject_is_none_when_nothing_matches() {
        let subjects = vec![user_subject("bob")];
        assert_eq!(first_applicable_subject("alice", &[], &subjects, ""), None);
    }
}
