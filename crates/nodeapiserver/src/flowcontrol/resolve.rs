//! Storage resolution for API Priority and Fairness — fetches real
//! `FlowSchema`/`PriorityLevelConfiguration` objects and identifies which
//! pair governs a request.
//!
//! The request gate receives the selected limited level's nominal
//! concurrency shares, aggregate share total, queue shape, queue length,
//! lending, borrowing, and reject policy.
//! This module labels every response with
//! the selected pair through the same real response headers upstream sets
//! (`k8s.io/api/flowcontrol/v1/types.go`'s own
//! `ResponseHeaderMatchedFlowSchemaUID`/
//! `ResponseHeaderMatchedPriorityLevelConfigurationUID` constants —
//! `"X-Kubernetes-PF-FlowSchema-UID"`/`"X-Kubernetes-PF-PriorityLevel-UID"`,
//! fetched and read directly).

use crate::flowcontrol::flow_schema::{flow_distinguisher, select_flow_schema, RequestDigest};
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
    pub flow_distinguisher: String,
    pub exempt: bool,
    pub priority_level: PriorityLevelConfig,
    pub priority_levels: Vec<PriorityLevelConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorityLevelConfig {
    pub uid: String,
    pub exempt: bool,
    pub nominal_concurrency_shares: usize,
    pub total_nominal_concurrency_shares: usize,
    pub queues: usize,
    pub hand_size: usize,
    pub queue_length_limit: usize,
    pub lendable_percent: usize,
    pub borrowing_limit_percent: Option<usize>,
    pub reject: bool,
}

const DEFAULT_NOMINAL_CONCURRENCY_SHARES: usize = 30;
const DEFAULT_QUEUES: usize = 64;
const DEFAULT_HAND_SIZE: usize = 8;
const DEFAULT_QUEUE_LENGTH_LIMIT: usize = 50;

/// Lists every real `FlowSchema`, selects the one that governs this
/// request (`flow_schema::select_flow_schema`'s own real precedence/
/// tie-break order), then fetches its referenced
/// `PriorityLevelConfiguration` by name. When `cache_registry` is supplied,
/// the two configuration lists and the selected object use their existing
/// synchronized watch caches, avoiding a nodestore round trip on every
/// request. A cache miss still falls through to storage, preserving the
/// normal startup/unsynced behavior of `rest::list`/`rest::get`.
///
/// `None` on any resolution failure (unknown resource, list/get error, no
/// matching `FlowSchema`, missing `uid`) — deliberately fails open (no
/// header gets set) rather than blocking the request, since the finite gate
/// can still apply its safe default budget when APF bookkeeping fails.
pub async fn select_for_request(
    storage: &mut StorageClient,
    digest: &RequestDigest<'_>,
    cache_registry: Option<&crate::cacher::CacheRegistry>,
) -> Option<Selected> {
    let flow_schema_cache =
        cache_registry.and_then(|registry| registry.get(GROUP, VERSION, "flowschemas"));
    let flow_schemas = match rest::list(
        storage,
        flow_schema_cache.as_ref(),
        GROUP,
        VERSION,
        "flowschemas",
        None,
        "",
        "",
        0,
        "",
    )
    .await
    {
        Ok(ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => return None,
    };
    let selected = select_flow_schema(&flow_schemas, digest)?;
    let flow_schema_uid = selected["metadata"]["uid"].as_str()?.to_string();
    let flow_distinguisher = flow_distinguisher(selected, digest);
    let pl_name = selected["spec"]["priorityLevelConfiguration"]["name"].as_str()?;

    let priority_level_cache = cache_registry
        .and_then(|registry| registry.get(GROUP, VERSION, "prioritylevelconfigurations"));
    let priority_level_objects = match rest::list(
        storage,
        priority_level_cache.as_ref(),
        GROUP,
        VERSION,
        "prioritylevelconfigurations",
        None,
        "",
        "",
        0,
        "",
    )
    .await
    {
        Ok(ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => return None,
    };
    let total_nominal_concurrency_shares = priority_level_objects
        .iter()
        .filter(|level| level["spec"]["type"].as_str().unwrap_or("Limited") == "Limited")
        .map(nominal_concurrency_shares)
        .sum::<usize>()
        .max(1);
    let priority_level = match priority_level_objects
        .iter()
        .find(|object| object["metadata"]["name"].as_str() == Some(pl_name))
    {
        Some(object) => (*object).clone(),
        None => match rest::get(
            storage,
            priority_level_cache.as_ref(),
            GROUP,
            VERSION,
            "prioritylevelconfigurations",
            None,
            pl_name,
        )
        .await
        {
            Ok(GetOutcome::Found(object)) => object,
            _ => return None,
        },
    };
    let priority_level_uid = priority_level["metadata"]["uid"].as_str()?.to_string();

    let priority_level = priority_level_config(
        &priority_level,
        priority_level_uid.clone(),
        total_nominal_concurrency_shares,
    );
    let exempt = priority_level.exempt;
    let mut priority_levels = priority_level_objects
        .iter()
        .filter_map(|object| {
            let uid = object["metadata"]["uid"].as_str()?.to_string();
            Some(priority_level_config(
                object,
                uid,
                total_nominal_concurrency_shares,
            ))
        })
        .collect::<Vec<_>>();
    if let Some(level) = priority_levels
        .iter_mut()
        .find(|level| level.uid == priority_level.uid)
    {
        *level = priority_level.clone();
    } else {
        priority_levels.push(priority_level.clone());
    }
    Some(Selected {
        flow_schema_uid,
        priority_level_uid,
        flow_distinguisher,
        exempt,
        priority_level,
        priority_levels,
    })
}

fn priority_level_config(priority_level: &Value, uid: String, total_nominal_concurrency_shares: usize) -> PriorityLevelConfig {
    if priority_level["spec"]["type"].as_str() == Some("Exempt") {
        return PriorityLevelConfig {
            uid,
            exempt: true,
            nominal_concurrency_shares: 0,
            total_nominal_concurrency_shares,
            queues: 1,
            hand_size: 1,
            queue_length_limit: 0,
            lendable_percent: 0,
            borrowing_limit_percent: None,
            reject: false,
        };
    }

    let queuing = &priority_level["spec"]["limited"]["limitResponse"]["queuing"];
    let queues = queuing["queues"]
        .as_u64()
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_QUEUES)
        .max(1);
    let hand_size = queuing["handSize"]
        .as_u64()
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_HAND_SIZE)
        .max(1)
        .min(queues);
    PriorityLevelConfig {
        uid,
        exempt: false,
        nominal_concurrency_shares: nominal_concurrency_shares(priority_level),
        total_nominal_concurrency_shares,
        queues,
        hand_size,
        queue_length_limit: queuing["queueLengthLimit"]
            .as_u64()
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_QUEUE_LENGTH_LIMIT),
        lendable_percent: priority_level["spec"]["limited"]["lendablePercent"]
            .as_u64()
            .map(|value| value as usize)
            .unwrap_or(0)
            .min(100),
        borrowing_limit_percent: priority_level["spec"]["limited"]["borrowingLimitPercent"]
            .as_u64()
            .map(|value| value as usize),
        reject: priority_level["spec"]["limited"]["limitResponse"]["type"].as_str() == Some("Reject"),
    }
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
    use serde_json::json;

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

    #[test]
    fn limited_priority_level_reads_shuffle_and_lending_configuration() {
        let object = json!({
            "spec": {
                "type": "Limited",
                "limited": {
                    "nominalConcurrencyShares": 7,
                    "lendablePercent": 25,
                    "borrowingLimitPercent": 150,
                    "limitResponse": {
                        "type": "Queue",
                        "queuing": {
                            "queues": 32,
                            "handSize": 4,
                            "queueLengthLimit": 12
                        }
                    }
                }
            }
        });
        let config = priority_level_config(&object, "limited".to_string(), 10);
        assert_eq!(config.nominal_concurrency_shares, 7);
        assert_eq!(config.queues, 32);
        assert_eq!(config.hand_size, 4);
        assert_eq!(config.queue_length_limit, 12);
        assert_eq!(config.lendable_percent, 25);
        assert_eq!(config.borrowing_limit_percent, Some(150));
    }
}
