//! Builds one real `audit.k8s.io/v1` `Event` document
//! (`staging/src/k8s.io/apiserver/pkg/apis/audit/v1/types.go`, release-1.34,
//! fetched and read directly) — the same JSON shape a real cluster's own
//! `--audit-log-path` writes one line of per request, at real upstream's
//! own `Metadata` level (the most common real default: everything about
//! *who did what to which object*, but not the request/response object
//! bodies themselves — those are `Request`/`RequestResponse` level,
//! genuinely more invasive and not built here).
//!
//! **Wired into `server::listener`**: `server::listener::handle_with_audit`
//! wraps every request, calling [`build_event`] once the response is
//! known and logging it via this crate's own `tracing` output (target
//! `"nodeapiserver::audit"`) — see that function's own doc comment for
//! exactly why the sink is "this crate's own log output," not a
//! dedicated file/webhook, and why wrapping the call site (rather than
//! threading an audit context out through `handle`'s own many early
//! returns) was the far less invasive place to add this.
//!
//! **One stage only**: real upstream emits up to four events per request
//! across real audit *stages* (`RequestReceived`, `ResponseStarted` —
//! long-running requests like `watch` only, `ResponseComplete`, `Panic`).
//! This builder only ever produces a single event labeled
//! `ResponseComplete` — accurate for every ordinary REST verb (the
//! request has genuinely finished by the time `handle_with_audit` logs
//! it), but a **named inaccuracy for `watch`**: a watch response is only
//! *starting* to stream when `handle` returns it (real upstream's own
//! `ResponseStarted` is the semantically correct stage there), not
//! complete — this builder has no way to know when a stream later ends
//! (that would need a hook into the response body's own completion, not
//! built), so a watch request's one logged event is stamped
//! `ResponseComplete` a little early, a real, narrow, honestly-named gap
//! rather than a silently wrong claim.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Real upstream's own `Level` constants — only `"Metadata"` is actually
/// produced by [`build_event`] today (see this module's own doc
/// comment), the others are real values a caller could reasonably ask
/// for once request/response body logging exists.
pub const LEVEL_METADATA: &str = "Metadata";

pub struct EventInput<'a> {
    pub audit_id: &'a str,
    pub request_uri: &'a str,
    pub verb: &'a str,
    /// Real upstream's own `authn/v1.UserInfo` shape: `username` +
    /// `groups` and the optional `uid` are the identity fields this crate's
    /// authenticators currently expose. `extra` remains unsupported.
    pub user_name: &'a str,
    pub user_uid: Option<&'a str>,
    pub user_groups: &'a [String],
    pub source_ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    /// `None` for a non-resource request (real upstream's own
    /// `ObjectRef` is `nil` there too).
    pub object_ref: Option<ObjectRef<'a>>,
    pub response_code: u16,
    pub annotations: Option<&'a BTreeMap<String, String>>,
    /// RFC3339 (real upstream uses `MicroTime`, sub-second precision —
    /// this crate's own `chrono`-based timestamps elsewhere in the code
    /// base are already RFC3339-second precision, so this is that same
    /// convention, not upstream's exact microsecond format; named,
    /// narrow, cosmetic-only gap).
    pub timestamp: &'a str,
}

pub struct ObjectRef<'a> {
    pub group: &'a str,
    pub resource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
    pub api_version: &'a str,
}

/// Real upstream's own `Event`, `Metadata` level, `ResponseComplete`
/// stage only (see this module's own doc comment for why).
pub fn build_event(input: &EventInput<'_>) -> Value {
    let mut event = json!({
        "kind": "Event",
        "apiVersion": "audit.k8s.io/v1",
        "level": LEVEL_METADATA,
        "auditID": input.audit_id,
        "stage": "ResponseComplete",
        "requestURI": input.request_uri,
        "verb": input.verb,
        "user": {
            "username": input.user_name,
            "groups": input.user_groups,
        },
        "requestReceivedTimestamp": input.timestamp,
        "stageTimestamp": input.timestamp,
        "responseStatus": {
            "code": input.response_code,
        },
    });

    if let Some(uid) = input.user_uid {
        event["user"]["uid"] = json!(uid);
    }

    if let Some(ip) = input.source_ip {
        event["sourceIPs"] = json!([ip]);
    }
    if let Some(agent) = input.user_agent {
        event["userAgent"] = json!(agent);
    }
    if let Some(obj) = &input.object_ref {
        event["objectRef"] = json!({
            "resource": obj.resource,
            "namespace": obj.namespace,
            "name": obj.name,
            "apiGroup": obj.group,
            "apiVersion": obj.api_version,
        });
    }
    if let Some(annotations) = input.annotations.filter(|annotations| !annotations.is_empty()) {
        event["annotations"] = json!(annotations);
    }

    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_input() -> EventInput<'static> {
        EventInput {
            audit_id: "11111111-1111-1111-1111-111111111111",
            request_uri: "/api/v1/namespaces/default/pods/web-1",
            verb: "get",
            user_name: "alice",
            user_uid: None,
            user_groups: &[],
            source_ip: None,
            user_agent: None,
            object_ref: None,
            response_code: 200,
            annotations: None,
            timestamp: "2026-08-20T12:00:00Z",
        }
    }

    #[test]
    fn build_event_carries_the_real_kind_and_group_version() {
        let event = build_event(&minimal_input());
        assert_eq!(event["kind"], "Event");
        assert_eq!(event["apiVersion"], "audit.k8s.io/v1");
        assert_eq!(event["level"], "Metadata");
        assert_eq!(event["stage"], "ResponseComplete");
    }

    #[test]
    fn build_event_carries_user_and_verb_and_response_code() {
        let event = build_event(&minimal_input());
        assert_eq!(event["user"]["username"], "alice");
        assert_eq!(event["verb"], "get");
        assert_eq!(event["responseStatus"]["code"], 200);
    }

    #[test]
    fn build_event_omits_optional_fields_when_absent() {
        let event = build_event(&minimal_input());
        assert!(event.get("sourceIPs").is_none());
        assert!(event.get("userAgent").is_none());
        assert!(event.get("objectRef").is_none());
    }

    #[test]
    fn build_event_includes_source_ip_and_user_agent_when_present() {
        let mut input = minimal_input();
        input.source_ip = Some("10.0.0.5");
        input.user_agent = Some("kubectl/v1.34.0");
        let event = build_event(&input);
        assert_eq!(event["sourceIPs"], json!(["10.0.0.5"]));
        assert_eq!(event["userAgent"], "kubectl/v1.34.0");
    }

    #[test]
    fn build_event_includes_a_real_object_ref_for_a_resource_request() {
        let mut input = minimal_input();
        input.object_ref = Some(ObjectRef { group: "apps", resource: "deployments", namespace: "default", name: "web", api_version: "v1" });
        let event = build_event(&input);
        assert_eq!(event["objectRef"]["apiGroup"], "apps");
        assert_eq!(event["objectRef"]["resource"], "deployments");
        assert_eq!(event["objectRef"]["namespace"], "default");
        assert_eq!(event["objectRef"]["name"], "web");
    }

    #[test]
    fn build_event_reports_multiple_user_groups() {
        let mut input = minimal_input();
        let groups = vec!["system:authenticated".to_string(), "developers".to_string()];
        input.user_groups = &groups;
        let event = build_event(&input);
        assert_eq!(event["user"]["groups"], json!(["system:authenticated", "developers"]));
    }

    #[test]
    fn build_event_includes_audit_annotations_when_present() {
        let mut input = minimal_input();
        let annotations = BTreeMap::from([(String::from("example.com/check"), String::from("failed"))]);
        input.annotations = Some(&annotations);
        let event = build_event(&input);
        assert_eq!(event["annotations"]["example.com/check"], "failed");
    }
}
