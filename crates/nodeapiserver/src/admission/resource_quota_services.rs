pub fn applies_to_service(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    group.is_empty()
        && resource == "services"
        && subresource.is_empty()
        && operation == crate::admission::attributes::Operation::Create
}

const TRACKED_SERVICE_RESOURCES: [&str; 3] =
    ["services", "services.nodeports", "services.loadbalancers"];

fn quota_applies_to_services(resource_quota: &Value) -> bool {
    hard_limits(resource_quota)
        .keys()
        .any(|k| TRACKED_SERVICE_RESOURCES.contains(&k.as_str()))
}

/// Real upstream's own `serviceEvaluator.Usage`, ported exactly: every
/// `Service` counts once toward `services`; `services.nodeports` counts
/// the number of ports that actually consume a node port (a `NodePort`
/// service always counts every port; a `LoadBalancer` service counts
/// every port unless it explicitly opted out of node-port allocation via
/// `spec.allocateLoadBalancerNodePorts: false`, in which case only ports
/// with an explicit `nodePort` value already set count — real upstream's
/// own `portsWithNodePorts`); `services.loadbalancers` counts 1 only for
/// `LoadBalancer`-type services. Unscoped quotas only, same posture as
/// [`check_pvc_create`] and for the same reason (real upstream's own
/// `serviceEvaluator.Matches` uses `generic.MatchesNoScopeFunc`
/// unconditionally — services never match any scope at all, no feature
/// gate involved).
fn service_usage(svc: &Value) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    usage.insert("services".to_string(), one);

    let ports = svc
        .get("spec")
        .and_then(|s| s.get("ports"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let port_count = Quantity::parse(&ports.len().to_string()).unwrap_or(Quantity::ZERO);
    let ports_with_node_port = || {
        let count = ports
            .iter()
            .filter(|p| {
                p.get("nodePort")
                    .and_then(Value::as_i64)
                    .is_some_and(|n| n != 0)
            })
            .count();
        Quantity::parse(&count.to_string()).unwrap_or(Quantity::ZERO)
    };

    match svc
        .get("spec")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("ClusterIP")
    {
        "NodePort" => {
            usage.insert("services.nodeports".to_string(), port_count);
        }
        "LoadBalancer" => {
            let allocates_node_ports = svc
                .get("spec")
                .and_then(|s| s.get("allocateLoadBalancerNodePorts"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            usage.insert(
                "services.nodeports".to_string(),
                if allocates_node_ports {
                    port_count
                } else {
                    ports_with_node_port()
                },
            );
            usage.insert("services.loadbalancers".to_string(), one);
        }
        _ => {}
    }
    usage
}

pub fn check_service_create(
    svc: &Value,
    existing_services: &[Value],
    resource_quotas: &[Value],
) -> Option<String> {
    let this_service_usage = service_usage(svc);

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes || !quota_applies_to_services(resource_quota) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_services {
            existing_usage = add_maps(&existing_usage, &service_usage(existing));
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_service_usage) {
            return Some(message);
        }
    }
    None
}

/// The persisted-`status.used` half of [`check_service_create`] — see
/// [`usage_after_pod_create`]'s own doc comment for the shape and
/// reasoning.
pub fn usage_after_service_create(
    svc: &Value,
    existing_services: &[Value],
    resource_quotas: &[Value],
) -> Vec<(String, BTreeMap<String, Quantity>)> {
    let this_service_usage = service_usage(svc);
    let mut updates = Vec::new();

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes || !quota_applies_to_services(resource_quota) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_services {
            existing_usage = add_maps(&existing_usage, &service_usage(existing));
        }
        let new_total = add_maps(&existing_usage, &this_service_usage);
        let name = resource_quota
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        updates.push((name, new_total));
    }
    updates
}

/// Real upstream's own `generic.ObjectCountQuotaResourceNameFor`: the
/// stable `count/<resource>` (core group) or `count/<resource>.<group>`
/// (every other group) quota-resource-name convention — the same one a
/// real cluster's own `kubectl create quota ... --hard=count/secrets=10`
/// or `count/deployments.apps=5` relies on.
pub fn count_quota_resource_name(group: &str, resource: &str) -> String {
    if group.is_empty() {
        format!("count/{resource}")
    } else {
        format!("count/{resource}.{group}")
    }
}

/// Real upstream's own generic `objectCountEvaluator` (real upstream
/// registers one of these for essentially every resource kind that
/// isn't a pod/PVC/service — this port doesn't need a per-resource
/// registration decision at all, since the `count/...` key it checks
/// against is fully generic over `(group, resource)`): forbids a
/// `CREATE` of any resource kind that would push the namespace's object
/// count for that kind over a `ResourceQuota`'s own
/// [`count_quota_resource_name`] hard limit, if one is set. Safe to call
/// unconditionally for any resource `CREATE` — a quota with no matching
/// `count/...` key simply has nothing to check, the same "nothing to
/// enforce" no-op every other unmatched case in this module already is.
/// Unscoped quotas only, matching real upstream's own generic evaluator
/// (no scope matching there either). Deliberately not called for pods/
/// PVCs/services in `server::listener` — those already get their own
/// legacy bare-name object-count tracking (`pods`/`persistentvolumeclaims`/
/// `services`) from their own dedicated evaluators above; running this
/// too would just be a redundant, wasted list round trip for those three,
/// not wrong.
pub fn check_object_count_create(
    group: &str,
    resource: &str,
    existing_objects: &[Value],
    resource_quotas: &[Value],
) -> Option<String> {
    let key = count_quota_resource_name(group, resource);
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    let existing_count =
        Quantity::parse(&existing_objects.len().to_string()).unwrap_or(Quantity::ZERO);
    let this_usage = BTreeMap::from([(key.clone(), one)]);
    let existing_usage = BTreeMap::from([(key, existing_count)]);

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes {
            continue;
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_usage) {
            return Some(message);
        }
    }
    None
}

/// The persisted-`status.used` half of [`check_object_count_create`] —
/// see [`usage_after_pod_create`]'s own doc comment for the shape and
/// reasoning. This closes the last remaining `ResourceQuota` evaluator
/// that didn't yet persist its own `status.used`: every evaluator this
/// crate has now does.
pub fn usage_after_object_count_create(
    group: &str,
    resource: &str,
    existing_objects: &[Value],
    resource_quotas: &[Value],
) -> Vec<(String, BTreeMap<String, Quantity>)> {
    let key = count_quota_resource_name(group, resource);
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    let existing_count =
        Quantity::parse(&existing_objects.len().to_string()).unwrap_or(Quantity::ZERO);
    let new_total = BTreeMap::from([(key, existing_count + one)]);
    let mut updates = Vec::new();

    for resource_quota in resource_quotas {
        if quota_has_any_scope_selectors(resource_quota) {
            continue;
        }
        let name = resource_quota
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        updates.push((name, new_total.clone()));
    }
    updates
}
