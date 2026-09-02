#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::attributes::Operation;
    use serde_json::json;

    fn pod_with_cpu_request(name: &str, cpu: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": cpu}}}]}})
    }

    fn quota(name: &str, hard: Value) -> Value {
        json!({"metadata": {"name": name}, "spec": {"hard": hard}})
    }

    #[test]
    fn applies_to_pod_create_only() {
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(!applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "apps", "deployments", ""));
    }

    #[test]
    fn counts_toward_quota_excludes_terminal_pods() {
        assert!(!counts_toward_quota(
            &json!({"status": {"phase": "Succeeded"}})
        ));
        assert!(!counts_toward_quota(
            &json!({"status": {"phase": "Failed"}})
        ));
        assert!(counts_toward_quota(
            &json!({"status": {"phase": "Running"}})
        ));
        assert!(
            counts_toward_quota(&json!({})),
            "no status at all (a just-created pod) counts"
        );
    }

    #[test]
    fn pod_usage_always_counts_the_pod_object() {
        let usage = pod_usage(&json!({"spec": {"containers": []}}));
        assert_eq!(usage["pods"].value(), 1);
    }

    #[test]
    fn pod_usage_tracks_cpu_and_memory_requests_and_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "200m", "memory": "256Mi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["cpu"].milli_value(), 100);
        assert_eq!(usage["requests.cpu"].milli_value(), 100);
        assert_eq!(usage["limits.cpu"].milli_value(), 200);
        assert_eq!(usage["memory"].value(), 128 * 1024 * 1024);
        assert_eq!(usage["limits.memory"].value(), 256 * 1024 * 1024);
    }

    #[test]
    fn pod_usage_tracks_ephemeral_storage_requests_and_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"ephemeral-storage": "1Gi"},
            "limits": {"ephemeral-storage": "2Gi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["ephemeral-storage"].value(), 1024 * 1024 * 1024);
        assert_eq!(
            usage["requests.ephemeral-storage"].value(),
            1024 * 1024 * 1024
        );
        assert_eq!(
            usage["limits.ephemeral-storage"].value(),
            2 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn an_ephemeral_storage_quota_is_enforced() {
        let mut pod = pod_with_cpu_request("new", "1m");
        pod["spec"]["containers"][0]["resources"]["requests"]["ephemeral-storage"] = json!("2Gi");
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"ephemeral-storage": "3Gi"}}}]}});
        let q = quota(
            "ephemeral-quota",
            json!({"requests.ephemeral-storage": "4Gi"}),
        );
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2Gi + 3Gi > 4Gi");
        assert!(denial.contains("requests.ephemeral-storage"));
    }

    #[test]
    fn usage_after_pod_create_sums_the_new_total_per_matching_quota() {
        let pod = pod_with_cpu_request("new", "1");
        let existing = pod_with_cpu_request("existing", "1");
        let q = quota("cpu-quota", json!({"requests.cpu": "10"}));
        let updates = usage_after_pod_create(&pod, &[existing], &[q]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "cpu-quota");
        assert_eq!(
            updates[0].1.get("requests.cpu"),
            Some(&Quantity::parse("2").unwrap())
        );
    }

    #[test]
    fn usage_after_pod_create_skips_a_non_matching_quota() {
        let pod = pod_with_cpu_request("new", "1");
        // Tracks only `count/services` -- never applies to a pod check at
        // all (same fixture `a_quota_tracking_only_service_count_never_applies_to_pods` uses).
        let q = quota("svc-quota", json!({"count/services": "1"}));
        assert!(usage_after_pod_create(&pod, &[], &[q]).is_empty());
    }

    #[test]
    fn pod_usage_tracks_hugepages_under_both_its_bare_and_requests_prefixed_name() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"hugepages-2Mi": "4Mi"},
            "limits": {"hugepages-2Mi": "4Mi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["hugepages-2Mi"].value(), 4 * 1024 * 1024);
        assert_eq!(usage["requests.hugepages-2Mi"].value(), 4 * 1024 * 1024);
        // Real upstream never tracks a separate limits.hugepages-* key.
        assert!(usage.get("limits.hugepages-2Mi").is_none());
    }

    #[test]
    fn a_hugepages_quota_is_enforced() {
        let pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "4Mi"}, "limits": {"hugepages-2Mi": "4Mi"}}}]}});
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "6Mi"}, "limits": {"hugepages-2Mi": "6Mi"}}}]}});
        let q = quota("hugepages-quota", json!({"requests.hugepages-2Mi": "8Mi"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("4Mi + 6Mi > 8Mi");
        assert!(denial.contains("requests.hugepages-2Mi"));
    }

    #[test]
    fn a_bare_hugepages_hard_limit_makes_the_quota_apply_too() {
        // spec.hard can name the resource with its bare hugepages-<size>
        // key too (not just the requests.-prefixed form) -- quota_applies
        // must recognize both, matching real upstream's own
        // podResourcePrefixes covering both prefixes.
        let pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "999Mi"}}}]}});
        let q = quota("hugepages-quota", json!({"hugepages-2Mi": "1Mi"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn pod_usage_tracks_an_extended_resource_under_its_requests_prefixed_name_only() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"nvidia.com/gpu": "2"},
            "limits": {"nvidia.com/gpu": "2"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["requests.nvidia.com/gpu"].milli_value(), 2000);
        // Real upstream never tracks a bare or limits.-prefixed key for an
        // extended resource -- overcommit isn't supported, so only the
        // requests.-prefixed form is ever recognized.
        assert!(usage.get("nvidia.com/gpu").is_none());
        assert!(usage.get("limits.nvidia.com/gpu").is_none());
    }

    #[test]
    fn an_extended_resource_quota_is_enforced() {
        let pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"nvidia.com/gpu": "2"}}}]}});
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"nvidia.com/gpu": "2"}}}]}});
        let q = quota("gpu-quota", json!({"requests.nvidia.com/gpu": "3"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2 + 2 > 3");
        assert!(denial.contains("requests.nvidia.com/gpu"));
    }

    #[test]
    fn a_native_resource_shaped_like_a_slash_name_is_not_treated_as_extended() {
        // kubernetes.io/-prefixed names are native, not extended -- a
        // requests.-prefixed hard limit for one must not be recognized by
        // the extended-resource path (it isn't tracked by this evaluator
        // at all, so such a quota simply never applies).
        let q = quota(
            "bogus-quota",
            json!({"requests.example.kubernetes.io/foo": "1"}),
        );
        assert!(!quota_applies(&q));
    }

    #[test]
    fn a_pod_within_quota_is_allowed() {
        let pod = pod_with_cpu_request("new", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_none());
    }

    #[test]
    fn a_pod_that_would_exceed_quota_is_denied() {
        let pod = pod_with_cpu_request("new", "600m");
        let existing = pod_with_cpu_request("existing", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("600m + 500m > 1 core");
        assert!(denial.contains("exceeded quota: compute-quota"));
        assert!(denial.contains("requests.cpu"));
    }

    #[test]
    fn a_terminal_existing_pod_does_not_count_against_quota() {
        let pod = pod_with_cpu_request("new", "600m");
        let mut existing = pod_with_cpu_request("existing", "500m");
        existing["status"] = json!({"phase": "Succeeded"});
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(
            check_pod_create(&pod, &[existing], &[q]).is_none(),
            "a Succeeded pod must not count against quota"
        );
    }

    #[test]
    fn a_quota_tracking_an_unrelated_resource_is_not_consulted() {
        let pod = pod_with_cpu_request("new", "999");
        let q = quota("services-quota", json!({"count/services": "5"}));
        assert!(
            check_pod_create(&pod, &[], &[q]).is_none(),
            "a quota tracking only count/services should never see a pod-only check"
        );
    }

    #[test]
    fn the_pods_count_limit_is_enforced_too() {
        let pod = pod_with_cpu_request("new", "1m");
        let existing = pod_with_cpu_request("existing", "1m");
        let q = quota("pod-count-quota", json!({"pods": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2 pods > hard limit of 1");
        assert!(denial.contains("pods="));
    }

    #[test]
    fn the_first_exceeded_quota_wins_not_an_aggregate_of_all() {
        let pod = pod_with_cpu_request("new", "999");
        let q1 = quota("q1", json!({"requests.cpu": "1"}));
        let q2 = quota("q2", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[], &[q1, q2]).unwrap();
        assert!(
            denial.contains("q1"),
            "the first quota in the list should be the one reported"
        );
    }

    #[test]
    fn compute_pod_qos_is_besteffort_with_no_requests_or_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        assert_eq!(compute_pod_qos(&pod), "BestEffort");
    }

    #[test]
    fn compute_pod_qos_is_guaranteed_when_every_container_matches_requests_to_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "100m", "memory": "128Mi"},
        }}]}});
        assert_eq!(compute_pod_qos(&pod), "Guaranteed");
    }

    #[test]
    fn compute_pod_qos_is_burstable_when_requests_and_limits_differ() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "200m", "memory": "256Mi"},
        }}]}});
        assert_eq!(compute_pod_qos(&pod), "Burstable");
    }

    #[test]
    fn compute_pod_qos_is_burstable_when_only_some_containers_set_limits() {
        let pod = json!({"spec": {"containers": [
            {"name": "c1", "resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "100m", "memory": "128Mi"}}},
            {"name": "c2", "resources": {"requests": {"cpu": "100m"}}},
        ]}});
        assert_eq!(compute_pod_qos(&pod), "Burstable");
    }

    #[test]
    fn is_terminating_reflects_active_deadline_seconds() {
        assert!(is_terminating(
            &json!({"spec": {"activeDeadlineSeconds": 30}})
        ));
        assert!(is_terminating(
            &json!({"spec": {"activeDeadlineSeconds": 0}})
        ));
        assert!(!is_terminating(&json!({"spec": {}})));
    }

    #[test]
    fn a_besteffort_scoped_quota_does_not_apply_to_a_non_besteffort_pod() {
        // A pod with requests set is Burstable, not BestEffort -- the
        // scope doesn't match, so the quota shouldn't even be consulted,
        // regardless of what its hard limit says.
        let burstable_pod = pod_with_cpu_request("new", "999");
        let q = json!({"metadata": {"name": "besteffort-quota"}, "spec": {"scopes": ["BestEffort"], "hard": {"requests.cpu": "1"}}});
        assert!(check_pod_create(&burstable_pod, &[], &[q]).is_none());
    }

    #[test]
    fn a_besteffort_scoped_quota_excludes_a_guaranteed_existing_pod_from_its_usage() {
        let besteffort_pod =
            json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        // Guaranteed: requests match limits exactly -- not BestEffort, so
        // this existing pod must not count toward a BestEffort-scoped
        // quota's usage even though it's a real pod in the namespace.
        let guaranteed_existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "1", "memory": "1Gi"},
            "limits": {"cpu": "1", "memory": "1Gi"},
        }}]}});
        let q = json!({"metadata": {"name": "besteffort-quota"}, "spec": {"scopes": ["BestEffort"], "hard": {"pods": "1"}}});
        // Only the new BestEffort pod counts (1 <= hard 1) -- the
        // Guaranteed existing pod is correctly excluded.
        assert!(
            check_pod_create(
                &besteffort_pod,
                &[guaranteed_existing.clone()],
                &[q.clone()]
            )
            .is_none()
        );

        // Add a second BestEffort existing pod -- now 2 BestEffort pods
        // total > hard 1, correctly denied.
        let besteffort_existing =
            json!({"metadata": {"name": "existing2"}, "spec": {"containers": [{"name": "c1"}]}});
        assert!(
            check_pod_create(
                &besteffort_pod,
                &[guaranteed_existing, besteffort_existing],
                &[q]
            )
            .is_some()
        );
    }

    #[test]
    fn a_terminating_scoped_quota_does_not_apply_to_a_non_terminating_pod() {
        let pod = pod_with_cpu_request("new", "999");
        let q = quota("terminating-quota", json!({"requests.cpu": "1"}));
        let mut scoped = q.clone();
        scoped["spec"]["scopes"] = json!(["Terminating"]);
        // The new pod has no activeDeadlineSeconds, so it is NotTerminating
        // -- the Terminating-scoped quota must not apply to it at all,
        // even though 999 cores would otherwise exceed it.
        assert!(check_pod_create(&pod, &[], &[scoped]).is_none());
    }

    #[test]
    fn a_terminating_scoped_quota_applies_to_a_terminating_pod() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["activeDeadlineSeconds"] = json!(30);
        let mut q = quota("terminating-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["Terminating"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn a_genuinely_unrecognized_scope_name_does_not_narrow_the_quota() {
        let pod = pod_with_cpu_request("new", "999");
        let mut q = quota("future-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["SomeFutureScope"]);
        assert!(
            check_pod_create(&pod, &[], &[q]).is_some(),
            "a genuinely unrecognized scope must not exempt the pod from an otherwise-applicable quota"
        );
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_does_not_apply_without_a_cross_namespace_term() {
        let pod = pod_with_cpu_request("new", "999");
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(
            check_pod_create(&pod, &[], &[q]).is_none(),
            "a pod with no affinity at all is not in scope for this quota"
        );
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_applies_when_a_required_term_names_namespaces() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"namespaces": ["other-ns"], "topologyKey": "kubernetes.io/hostname"}]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_applies_for_a_preferred_antiaffinity_namespace_selector()
     {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [
            {"weight": 1, "podAffinityTerm": {"namespaceSelector": {}, "topologyKey": "kubernetes.io/hostname"}},
        ]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn an_ordinary_same_namespace_affinity_term_does_not_count_as_cross_namespace() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"topologyKey": "kubernetes.io/hostname"}]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(
            check_pod_create(&pod, &[], &[q]).is_none(),
            "a term with no namespaces/namespaceSelector must not count as cross-namespace"
        );
    }

    #[test]
    fn a_priorityclass_scoped_quota_applies_only_to_pods_with_a_priority_class() {
        let mut with_priority = pod_with_cpu_request("new", "999");
        with_priority["spec"]["priorityClassName"] = json!("high");
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["PriorityClass"]);
        assert!(
            check_pod_create(&with_priority, &[], &[q.clone()]).is_some(),
            "a pod with a priority class name must be checked"
        );

        let without_priority = pod_with_cpu_request("new", "999");
        assert!(
            check_pod_create(&without_priority, &[], &[q]).is_none(),
            "a pod with no priority class name is out of scope for this quota"
        );
    }

    fn pvc_with_storage(name: &str, storage: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"resources": {"requests": {"storage": storage}}}})
    }

    #[test]
    fn applies_to_pvc_create_only() {
        use crate::admission::attributes::Operation;
        assert!(applies_to_pvc(
            Operation::Create,
            "",
            "persistentvolumeclaims",
            ""
        ));
        assert!(!applies_to_pvc(
            Operation::Update,
            "",
            "persistentvolumeclaims",
            ""
        ));
        assert!(!applies_to_pvc(
            Operation::Create,
            "",
            "persistentvolumeclaims",
            "status"
        ));
    }

    #[test]
    fn pvc_usage_always_counts_the_claim_object() {
        let usage = pvc_usage(&json!({"spec": {}}));
        assert_eq!(usage["persistentvolumeclaims"].value(), 1);
        assert!(usage.get("requests.storage").is_none());
    }

    #[test]
    fn pvc_usage_tracks_requested_storage() {
        let usage = pvc_usage(&pvc_with_storage("data", "10Gi"));
        assert_eq!(usage["requests.storage"].value(), 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_pvc_within_storage_quota_is_allowed() {
        let pvc = pvc_with_storage("new", "5Gi");
        let q = quota("storage-quota", json!({"requests.storage": "10Gi"}));
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none());
    }

    #[test]
    fn a_pvc_that_would_exceed_storage_quota_is_denied() {
        let pvc = pvc_with_storage("new", "8Gi");
        let existing = pvc_with_storage("existing", "5Gi");
        let q = quota("storage-quota", json!({"requests.storage": "10Gi"}));
        let denial = check_pvc_create(&pvc, &[existing], &[q]).expect("8Gi + 5Gi > 10Gi");
        assert!(denial.contains("exceeded quota: storage-quota"));
        assert!(denial.contains("requests.storage"));
    }

    #[test]
    fn the_pvc_count_limit_is_enforced_too() {
        let pvc = pvc_with_storage("new", "1Gi");
        let existing = pvc_with_storage("existing", "1Gi");
        let q = quota("pvc-count-quota", json!({"persistentvolumeclaims": "1"}));
        let denial = check_pvc_create(&pvc, &[existing], &[q]).expect("2 PVCs > hard limit of 1");
        assert!(denial.contains("persistentvolumeclaims="));
    }

    #[test]
    fn a_pvc_quota_tracking_an_unrelated_resource_is_not_consulted() {
        let pvc = pvc_with_storage("new", "999Gi");
        let q = quota("pods-quota", json!({"pods": "5"}));
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none());
    }

    #[test]
    fn a_scoped_quota_does_not_apply_to_pvcs_at_all() {
        let pvc = pvc_with_storage("new", "999Gi");
        let mut q = quota("scoped-quota", json!({"requests.storage": "1Gi"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(
            check_pvc_create(&pvc, &[], &[q]).is_none(),
            "a scoped quota must not apply to PVCs at all, matching real upstream's stable-feature-gate-off default"
        );
    }

    #[test]
    fn pvc_usage_tracks_a_storage_class_scoped_key_too_when_the_pvc_names_one() {
        let mut pvc = pvc_with_storage("data", "10Gi");
        pvc["spec"]["storageClassName"] = json!("gold");
        let usage = pvc_usage(&pvc);
        assert_eq!(usage["persistentvolumeclaims"].value(), 1);
        assert_eq!(
            usage["gold.storageclass.storage.k8s.io/persistentvolumeclaims"].value(),
            1
        );
        assert_eq!(usage["requests.storage"].value(), 10 * 1024 * 1024 * 1024);
        assert_eq!(
            usage["gold.storageclass.storage.k8s.io/requests.storage"].value(),
            10 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn pvc_usage_has_no_storage_class_scoped_keys_when_the_pvc_names_no_class() {
        let usage = pvc_usage(&pvc_with_storage("data", "10Gi"));
        assert!(
            usage
                .keys()
                .all(|k| !k.contains("storageclass.storage.k8s.io"))
        );
    }

    #[test]
    fn a_storage_class_scoped_quota_is_enforced() {
        let mut pvc = pvc_with_storage("new", "8Gi");
        pvc["spec"]["storageClassName"] = json!("gold");
        let mut existing = pvc_with_storage("existing", "5Gi");
        existing["spec"]["storageClassName"] = json!("gold");
        let q = quota(
            "gold-quota",
            json!({"gold.storageclass.storage.k8s.io/requests.storage": "10Gi"}),
        );
        let denial = check_pvc_create(&pvc, &[existing], &[q])
            .expect("8Gi + 5Gi > 10Gi under the gold class");
        assert!(denial.contains("gold.storageclass.storage.k8s.io/requests.storage"));
    }

    #[test]
    fn a_storage_class_scoped_quota_ignores_a_pvc_in_a_different_class() {
        let mut pvc = pvc_with_storage("new", "999Gi");
        pvc["spec"]["storageClassName"] = json!("bronze");
        let q = quota(
            "gold-quota",
            json!({"gold.storageclass.storage.k8s.io/requests.storage": "1Gi"}),
        );
        assert!(
            check_pvc_create(&pvc, &[], &[q]).is_none(),
            "a bronze-class PVC must not be charged against a gold-scoped quota key"
        );
    }

    #[test]
    fn usage_after_pvc_create_sums_the_new_total() {
        let pvc = pvc_with_storage("new", "1Gi");
        let existing = pvc_with_storage("existing", "2Gi");
        let q = quota("pvc-quota", json!({"requests.storage": "10Gi"}));
        let updates = usage_after_pvc_create(&pvc, &[existing], &[q]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "pvc-quota");
        assert_eq!(
            updates[0].1.get("requests.storage"),
            Some(&Quantity::parse("3Gi").unwrap())
        );
    }

    fn cluster_ip_service(name: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"type": "ClusterIP", "ports": [{"port": 80}]}})
    }

    #[test]
    fn applies_to_service_create_only() {
        assert!(applies_to_service(Operation::Create, "", "services", ""));
        assert!(!applies_to_service(Operation::Update, "", "services", ""));
        assert!(!applies_to_service(
            Operation::Create,
            "",
            "services",
            "status"
        ));
    }

    #[test]
    fn service_usage_always_counts_the_service_object() {
        let usage = service_usage(&cluster_ip_service("svc"));
        assert_eq!(usage["services"].value(), 1);
        assert!(usage.get("services.nodeports").is_none());
        assert!(usage.get("services.loadbalancers").is_none());
    }

    #[test]
    fn service_usage_counts_nodeports_for_a_nodeport_service() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {"type": "NodePort", "ports": [{"port": 80}, {"port": 443}]}});
        let usage = service_usage(&svc);
        assert_eq!(usage["services.nodeports"].value(), 2);
        assert!(usage.get("services.loadbalancers").is_none());
    }

    #[test]
    fn service_usage_counts_loadbalancer_and_nodeports_by_default() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {"type": "LoadBalancer", "ports": [{"port": 80}]}});
        let usage = service_usage(&svc);
        assert_eq!(usage["services.loadbalancers"].value(), 1);
        assert_eq!(usage["services.nodeports"].value(), 1);
    }

    #[test]
    fn service_usage_only_counts_explicit_nodeports_when_allocation_is_disabled() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {
            "type": "LoadBalancer",
            "allocateLoadBalancerNodePorts": false,
            "ports": [{"port": 80, "nodePort": 30080}, {"port": 443}],
        }});
        let usage = service_usage(&svc);
        assert_eq!(
            usage["services.nodeports"].value(),
            1,
            "only the port with an explicit nodePort counts"
        );
    }

    #[test]
    fn a_service_within_quota_is_allowed() {
        let svc = cluster_ip_service("new");
        let q = quota("service-quota", json!({"services": "5"}));
        assert!(check_service_create(&svc, &[], &[q]).is_none());
    }

    #[test]
    fn a_nodeport_service_that_would_exceed_quota_is_denied() {
        let svc = json!({"metadata": {"name": "new"}, "spec": {"type": "NodePort", "ports": [{"port": 80}, {"port": 443}]}});
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"type": "NodePort", "ports": [{"port": 8080}]}});
        let q = quota("nodeport-quota", json!({"services.nodeports": "2"}));
        let denial = check_service_create(&svc, &[existing], &[q]).expect("2 + 1 > 2");
        assert!(denial.contains("services.nodeports"));
    }

    #[test]
    fn a_scoped_quota_does_not_apply_to_services_at_all() {
        let svc = cluster_ip_service("new");
        let mut q = quota("scoped-quota", json!({"services": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(check_service_create(&svc, &[], &[q]).is_none());
    }

    #[test]
    fn usage_after_service_create_sums_the_new_total() {
        let svc = cluster_ip_service("new");
        let existing = cluster_ip_service("existing");
        let q = quota("svc-quota", json!({"services": "10"}));
        let updates = usage_after_service_create(&svc, &[existing], &[q]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "svc-quota");
        assert_eq!(
            updates[0].1.get("services"),
            Some(&Quantity::parse("2").unwrap())
        );
    }

    #[test]
    fn count_quota_resource_name_uses_the_real_convention() {
        assert_eq!(count_quota_resource_name("", "secrets"), "count/secrets");
        assert_eq!(
            count_quota_resource_name("apps", "deployments"),
            "count/deployments.apps"
        );
    }

    #[test]
    fn object_count_quota_allows_a_create_within_the_hard_limit() {
        let q = quota("secrets-quota", json!({"count/secrets": "5"}));
        let existing: Vec<Value> = (0..3).map(|_| json!({})).collect();
        assert!(check_object_count_create("", "secrets", &existing, &[q]).is_none());
    }

    #[test]
    fn usage_after_object_count_create_adds_one_for_the_new_object() {
        let q = quota("secrets-quota", json!({"count/secrets": "10"}));
        let existing: Vec<Value> = (0..3).map(|_| json!({})).collect();
        let updates = usage_after_object_count_create("", "secrets", &existing, &[q]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "secrets-quota");
        assert_eq!(
            updates[0].1.get("count/secrets"),
            Some(&Quantity::parse("4").unwrap())
        );
    }

    #[test]
    fn object_count_quota_denies_a_create_that_would_exceed_the_hard_limit() {
        let q = quota("secrets-quota", json!({"count/secrets": "5"}));
        let existing: Vec<Value> = (0..5).map(|_| json!({})).collect();
        let denial = check_object_count_create("", "secrets", &existing, &[q])
            .expect("5 existing + 1 new > hard limit of 5");
        assert!(denial.contains("count/secrets"));
    }

    #[test]
    fn object_count_quota_uses_the_group_qualified_key_for_a_non_core_resource() {
        let q = quota("deploy-quota", json!({"count/deployments.apps": "1"}));
        let existing = vec![json!({})];
        let denial = check_object_count_create("apps", "deployments", &existing, &[q])
            .expect("1 existing + 1 new > hard limit of 1");
        assert!(denial.contains("count/deployments.apps"));
    }

    #[test]
    fn object_count_quota_ignores_an_unrelated_hard_limit() {
        let q = quota("secrets-quota", json!({"count/configmaps": "1"}));
        let existing = vec![json!({}), json!({}), json!({})];
        assert!(check_object_count_create("", "secrets", &existing, &[q]).is_none());
    }

    #[test]
    fn object_count_quota_does_not_apply_when_scoped() {
        let mut q = quota("secrets-quota", json!({"count/secrets": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(check_object_count_create("", "secrets", &[], &[q]).is_none());
    }

    #[test]
    fn a_scope_selector_priority_class_in_matches_only_the_named_classes() {
        let mut high = pod_with_cpu_request("new", "999");
        high["spec"]["priorityClassName"] = json!("high");
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "In", "values": ["high", "critical"]}]});
        assert!(
            check_pod_create(&high, &[], &[q.clone()]).is_some(),
            "priorityClassName=high is in the In list, so the quota applies"
        );

        let mut low = pod_with_cpu_request("new", "999");
        low["spec"]["priorityClassName"] = json!("low");
        assert!(
            check_pod_create(&low, &[], &[q]).is_none(),
            "priorityClassName=low is not in the In list, so the quota must not apply"
        );
    }

    #[test]
    fn a_scope_selector_priority_class_notin_matches_absent_or_unlisted() {
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "NotIn", "values": ["high"]}]});

        let no_priority = pod_with_cpu_request("new", "999");
        assert!(
            check_pod_create(&no_priority, &[], &[q.clone()]).is_some(),
            "no priority class name at all: NotIn matches (key absent)"
        );

        let mut low = pod_with_cpu_request("new", "999");
        low["spec"]["priorityClassName"] = json!("low");
        assert!(
            check_pod_create(&low, &[], &[q.clone()]).is_some(),
            "priorityClassName=low is not in the disallowed set, so NotIn matches"
        );

        let mut high = pod_with_cpu_request("new", "999");
        high["spec"]["priorityClassName"] = json!("high");
        assert!(
            check_pod_create(&high, &[], &[q]).is_none(),
            "priorityClassName=high is in the disallowed set, so NotIn must not match"
        );
    }

    #[test]
    fn a_scope_selector_priority_class_doesnotexist_requires_no_priority_class() {
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "DoesNotExist"}]});

        let no_priority = pod_with_cpu_request("new", "999");
        assert!(check_pod_create(&no_priority, &[], &[q.clone()]).is_some());

        let mut with_priority = pod_with_cpu_request("new", "999");
        with_priority["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&with_priority, &[], &[q]).is_none());
    }

    #[test]
    fn scopes_and_scope_selector_combine_with_and_semantics() {
        // BestEffort (from spec.scopes) AND PriorityClass=high (from
        // spec.scopeSelector) -- real upstream's own
        // getScopeSelectorsFromQuota concatenation, both must match.
        let mut q = quota("combo-quota", json!({"pods": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "In", "values": ["high"]}]});

        // BestEffort but wrong priority class: scopeSelector fails.
        let mut besteffort_wrong_priority =
            json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        besteffort_wrong_priority["spec"]["priorityClassName"] = json!("low");
        assert!(check_pod_create(&besteffort_wrong_priority, &[], &[q.clone()]).is_none());

        // Not BestEffort (has cpu requests) even with the right priority
        // class: spec.scopes fails.
        let mut burstable_right_priority = pod_with_cpu_request("new", "1m");
        burstable_right_priority["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&burstable_right_priority, &[], &[q.clone()]).is_none());

        // Both match.
        let mut both_match =
            json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        both_match["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&both_match, &[], &[q]).is_some());
    }

    #[test]
    fn a_scope_selector_alone_makes_the_pvc_evaluator_treat_the_quota_as_scoped() {
        let pvc = pvc_with_storage("new", "999Gi");
        let mut q = quota("scoped-quota", json!({"requests.storage": "1Gi"}));
        q["spec"]["scopeSelector"] =
            json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "Exists"}]});
        assert!(
            check_pvc_create(&pvc, &[], &[q]).is_none(),
            "a spec.scopeSelector alone (no spec.scopes) must also make the PVC evaluator skip this quota"
        );
    }
}
