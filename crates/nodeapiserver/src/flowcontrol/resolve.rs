//! Storage resolution for API Priority and Fairness — fetches real
//! `FlowSchema`/`PriorityLevelConfiguration` objects and identifies which
//! pair governs a request, the storage-backed half `flow_schema`'s own
//! module doc comment named as not yet built.
//!
//! **Still no concurrency-limiting/queuing** — real upstream's own
//! fair-queuing/seat-borrowing algorithm remains unstarted, named
//! honestly. What this *does* do, matching real upstream's own observable
//! behavior even before any limiting exists: label every response with
//! which `FlowSchema`/`PriorityLevelConfiguration` would have governed it,
//! via the same real response headers upstream sets
//! (`k8s.io/api/flowcontrol/v1/types.go`'s own
//! `ResponseHeaderMatchedFlowSchemaUID`/
//! `ResponseHeaderMatchedPriorityLevelConfigurationUID` constants —
//! `"X-Kubernetes-PF-FlowSchema-UID"`/`"X-Kubernetes-PF-PriorityLevel-UID"`,
//! fetched and read directly). Every request still runs at full priority,
//! never queued or rejected by this.

use crate::flowcontrol::flow_schema::{select_flow_schema, RequestDigest};
use crate::server::rest::{self, GetOutcome, ListOutcome};
use crate::storage::client::StorageClient;

const GROUP: &str = "flowcontrol.apiserver.k8s.io";
const VERSION: &str = "v1";

/// The real header names real upstream sets, ported verbatim.
pub const FLOW_SCHEMA_UID_HEADER: &str = "X-Kubernetes-PF-FlowSchema-UID";
pub const PRIORITY_LEVEL_UID_HEADER: &str = "X-Kubernetes-PF-PriorityLevel-UID";

pub struct Selected {
    pub flow_schema_uid: String,
    pub priority_level_uid: String,
}

/// Lists every real `FlowSchema`, selects the one that governs this
/// request (`flow_schema::select_flow_schema`'s own real precedence/
/// tie-break order), then fetches its referenced
/// `PriorityLevelConfiguration` by name. `None` on any resolution failure
/// (unknown resource, list/get error, no matching `FlowSchema`, missing
/// `uid`) — deliberately fails open (no header gets set) rather than
/// blocking the request, since nothing in this build enforces the result
/// yet; a request should never be denied because APF bookkeeping itself
/// failed.
pub async fn select_for_request(storage: &mut StorageClient, digest: &RequestDigest<'_>) -> Option<Selected> {
    let flow_schemas = match rest::list(storage, None, GROUP, VERSION, "flowschemas", None, "", "").await {
        Ok(ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => return None,
    };
    let selected = select_flow_schema(&flow_schemas, digest)?;
    let flow_schema_uid = selected["metadata"]["uid"].as_str()?.to_string();
    let pl_name = selected["spec"]["priorityLevelConfiguration"]["name"].as_str()?;

    let priority_level = match rest::get(storage, None, GROUP, VERSION, "prioritylevelconfigurations", None, pl_name).await {
        Ok(GetOutcome::Found(obj)) => obj,
        _ => return None,
    };
    let priority_level_uid = priority_level["metadata"]["uid"].as_str()?.to_string();

    Some(Selected { flow_schema_uid, priority_level_uid })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest<'a>() -> RequestDigest<'a> {
        RequestDigest {
            user_name: "alice",
            user_groups: &[],
            verb: "get",
            is_resource_request: true,
            api_group: "",
            resource: "pods",
            subresource: "",
            namespace: "default",
            path: "",
        }
    }

    #[test]
    fn header_names_match_real_upstream() {
        // Ported verbatim from k8s.io/api/flowcontrol/v1/types.go's own
        // ResponseHeaderMatchedFlowSchemaUID/
        // ResponseHeaderMatchedPriorityLevelConfigurationUID constants —
        // pinned here so a future edit can't silently drift from them.
        assert_eq!(FLOW_SCHEMA_UID_HEADER, "X-Kubernetes-PF-FlowSchema-UID");
        assert_eq!(PRIORITY_LEVEL_UID_HEADER, "X-Kubernetes-PF-PriorityLevel-UID");
    }

    #[test]
    fn digest_helper_builds_a_resource_request() {
        let d = digest();
        assert!(d.is_resource_request);
        assert_eq!(d.resource, "pods");
    }
}
