//! `SelfSubjectReview` — real upstream's own `authentication.k8s.io/v1`
//! "whoami" endpoint (`kubectl auth whoami`, GA since 1.28): reflects the
//! caller's own already-authenticated identity back in
//! `status.userInfo`. A genuine virtual resource, same as
//! `authz::sar`'s review kinds — never persisted, real upstream's own
//! `pkg/registry/authentication/selfsubjectreview` is a synthetic REST
//! connector too.
//!
//! No new authentication logic at all: this only reflects whatever
//! `authn::x509` (or, for an unauthenticated caller, the real anonymous
//! user/group convention `server::listener` already uses everywhere
//! else) already produced.

use serde_json::Value;

/// Real upstream's own `UserInfo` shape. `uid`/`extra` are always empty
/// here — this crate's only identity source (`authn::x509`) has neither
/// (a client cert carries no UID, and `extra` is real upstream's own
/// authenticator-specific side-channel data, e.g. impersonation headers,
/// none of which this crate's x509 path produces).
pub fn build_status(username: &str, groups: &[String]) -> Value {
    serde_json::json!({"userInfo": {"username": username, "groups": groups}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_status_reflects_username_and_groups() {
        let status = build_status("alice", &["devs".to_string(), "system:authenticated".to_string()]);
        assert_eq!(status, json!({"userInfo": {"username": "alice", "groups": ["devs", "system:authenticated"]}}));
    }

    #[test]
    fn build_status_handles_no_groups() {
        let status = build_status("system:anonymous", &[]);
        assert_eq!(status["userInfo"]["groups"], json!([]));
    }
}
