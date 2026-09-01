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
use std::collections::BTreeMap;

/// Real upstream's own `UserInfo` shape. `extra` remains empty because this
/// crate does not implement impersonation headers or another authenticator-
/// specific side channel, but authenticators that know a UID preserve it.
pub fn build_status(
    username: &str,
    uid: Option<&str>,
    groups: &[String],
    extra: &BTreeMap<String, Vec<String>>,
) -> Value {
    let mut user_info = serde_json::json!({"username": username, "groups": groups, "extra": extra});
    if let Some(uid) = uid {
        user_info["uid"] = serde_json::json!(uid);
    }
    serde_json::json!({"userInfo": user_info})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_status_reflects_username_and_groups() {
        let status = build_status("alice", None, &["devs".to_string(), "system:authenticated".to_string()], &BTreeMap::new());
        assert_eq!(status, json!({"userInfo": {"username": "alice", "groups": ["devs", "system:authenticated"], "extra": {}}}));
    }

    #[test]
    fn build_status_handles_no_groups() {
        let status = build_status("system:anonymous", None, &[], &BTreeMap::new());
        assert_eq!(status["userInfo"]["groups"], json!([]));
    }

    #[test]
    fn build_status_preserves_an_authenticator_uid() {
        let status = build_status("bootstrap-user", Some("uid-1"), &[], &BTreeMap::new());
        assert_eq!(status["userInfo"]["uid"], json!("uid-1"));
    }
}
