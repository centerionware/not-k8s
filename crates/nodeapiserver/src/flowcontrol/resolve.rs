//! Storage resolution for API Priority and Fairness — fetches real
//! `FlowSchema`/`PriorityLevelConfiguration` objects and identifies which
//! pair governs a request, the storage-backed half `flow_schema`'s own
//! module doc comment named as not yet built.
//!
//! The request gate receives the selected limited level's nominal
//! concurrency shares, aggregate share total, queue length, and reject
//! policy. The full upstream shuffle-sharded fair queue and seat borrowing
//! remain separate refinements. This module still labels every response with
//! the selected pair through the same real response headers upstream sets
//! (`k8s.io/api/flowcontrol/v1/types.go`'s own
//! `ResponseHeaderMatchedFlowSchemaUID`/
//! `ResponseHeaderMatchedPriorityLevelConfigurationUID` constants —
//! `"X-Kubernetes-PF-FlowSchema-UID"`/`"X-Kubernetes-PF-PriorityLevel-UID"`,
//! fetched and read directly).

use crate::flowcontrol::flow_schema::{select_flow_schema, RequestDigest};
use crate::server::rest::{self, GetOutcome, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;

const GROUP: &str = "flowcontrol.apiserver.k8s.io";
const VERSION: &str = "v1";

/// The real header names real upstream sets, ported verbatim.
pub const FLOW_SCHEMA_UID_HEADER: &str = "X-Kubernetes-PF-FlowSchema-UID";
pub const PRIORITY_LEVEL_UID_HEADER: &str = "X-Kubernetes-PF-PriorityLevel-UID";

pub struct Selected {
    pub flow_schema_uid: String,
    pub priority_level_uid: String,
    pub exempt: bool,
    pub priority_level: PriorityLevelConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorityLevelConfig {
    pub uid: String,
    pub exempt: bool,
    pub nominal_concurrency_shares: usize,
    pub total_nominal_concurrency_shares: usize,
    pub queue_length_limit: usize,
    pub reject: bool,
}

const DEFAULT_NOMINAL_CONCURRENCY_SHARES: usize = 30;
const DEFAULT_QUEUE_LENGTH_LIMIT: usize = 50;

/// Lists every real `FlowSchema`, selects the one that governs this
/// request (`flow_schema::select_flow_schema`'s own real precedence/
/// tie-break order), then fetches its referenced
/// `PriorityLevelConfiguration` by name. `None` on any resolution failure
/// (unknown resource, list/get error, no matching `FlowSchema`, missing
/// `uid`) — deliberately fails open (no header gets set) rather than
/// blocking the request, since the finite gate can still apply its safe
/// default budget when APF bookkeeping fails.
pub async fn select_for_request(storage: &mut StorageClient, digest: &RequestDigest<'_>) -> Option<Selected> {
    let flow_schemas = match rest::list(storage, None, GROUP, VERSION, "flowschemas", None, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => return None,
    };
    let selected = select_flow_schema(&flow_schemas, digest)?;
    let flow_schema_uid = selected["metadata"]["uid"].as_str()?.to_string();
    let pl_name = selected["spec"]["priorityLevelConfiguration"]["name"].as_str()?;

    let priority_levels = match rest::list(storage, None, GROUP, VERSION, "prioritylevelconfigurations", None, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => return None,
    };
    let total_nominal_concurrency_shares = priority_levels
        .iter()
        .filter(|level| level["spec"]["type"].as_str().unwrap_or("Limited") == "Limited")
        .map(nominal_concurrency_shares)
        .sum::<usize>()
        .max(1);
    let priority_level = match rest::get(storage, None, GROUP, VERSION, "prioritylevelconfigurations", None, pl_name).await {
        Ok(GetOutcome::Found(obj)) => obj,
        _ => return None,
    };
    let priority_level_uid = priority_level["metadata"]["uid"].as_str()?.to_string();

    let exempt = priority_level["spec"]["type"].as_str() == Some("Exempt");
    let priority_level = if exempt {
        PriorityLevelConfig {
            uid: priority_level_uid.clone(),
            exempt: true,
            nominal_concurrency_shares: 0,
            total_nominal_concurrency_shares,
            queue_length_limit: 0,
            reject: false,
        }
    } else {
        PriorityLevelConfig {
            uid: priority_level_uid.clone(),
            exempt: false,
            nominal_concurrency_shares: nominal_concurrency_shares(&priority_level),
            total_nominal_concurrency_shares,
            queue_length_limit: priority_level["spec"]["limited"]["limitResponse"]["queuing"]["queueLengthLimit"]
                .as_u64()
                .map(|value| value as usize)
                .unwrap_or(DEFAULT_QUEUE_LENGTH_LIMIT),
            reject: priority_level["spec"]["limited"]["limitResponse"]["type"].as_str() == Some("Reject"),
        }
    };
    Some(Selected { flow_schema_uid, priority_level_uid, exempt, priority_level })
}

fn nominal_concurrency_shares(priority_level: &Value) -> usize {
    priority_level["spec"]["limited"]["nominalConcurrencyShares"]
        .as_u64()
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_NOMINAL_CONCURRENCY_SHARES)
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
