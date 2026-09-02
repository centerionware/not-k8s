mod tests {
    use super::*;

    #[test]
    fn a_single_object_becomes_a_one_row_table() {
        let pod =
            json!({"metadata": {"name": "web-1", "creationTimestamp": "2026-01-01T00:00:00Z"}});
        let table = convert_to_table(&pod);
        assert_eq!(table["kind"], "Table");
        assert_eq!(table["apiVersion"], "meta.k8s.io/v1");
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["cells"], json!(["web-1", "2026-01-01T00:00:00Z"]));
        assert_eq!(rows[0]["object"], pod);
    }

    #[test]
    fn column_definitions_are_exactly_name_and_created_at() {
        let table = convert_to_table(&json!({"metadata": {"name": "x"}}));
        let cols = table["columnDefinitions"].as_array().unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0]["name"], "Name");
        assert_eq!(cols[0]["format"], "name");
        assert_eq!(cols[1]["name"], "Created At");
        assert_eq!(cols[1]["type"], "date");
    }

    #[test]
    fn a_list_shaped_object_produces_one_row_per_item() {
        let list = json!({
            "metadata": {"resourceVersion": "42"},
            "items": [
                {"metadata": {"name": "a", "creationTimestamp": "2026-01-01T00:00:00Z"}},
                {"metadata": {"name": "b", "creationTimestamp": "2026-01-02T00:00:00Z"}},
            ],
        });
        let table = convert_to_table(&list);
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["cells"], json!(["a", "2026-01-01T00:00:00Z"]));
        assert_eq!(rows[1]["cells"], json!(["b", "2026-01-02T00:00:00Z"]));
    }

    #[test]
    fn list_metadata_is_carried_through_to_the_table() {
        let list = json!({
            "metadata": {"resourceVersion": "42", "continue": "abc", "remainingItemCount": 3},
            "items": [],
        });
        let table = convert_to_table(&list);
        assert_eq!(table["metadata"]["resourceVersion"], "42");
        assert_eq!(table["metadata"]["continue"], "abc");
        assert_eq!(table["metadata"]["remainingItemCount"], 3);
    }

    #[test]
    fn an_empty_list_has_no_rows_but_is_still_a_valid_table() {
        let list = json!({"metadata": {}, "items": []});
        let table = convert_to_table(&list);
        assert_eq!(table["rows"], json!([]));
    }

    #[test]
    fn a_single_object_has_no_table_metadata_field_at_all() {
        // Only Lists carry ResourceVersion/Continue/RemainingItemCount —
        // a single object's own metadata isn't the same kind of thing and
        // must not leak into table["metadata"].
        let pod = json!({"metadata": {"name": "web-1", "resourceVersion": "7"}});
        let table = convert_to_table(&pod);
        assert!(table.get("metadata").is_none());
    }

    #[test]
    fn an_item_missing_creation_timestamp_gets_a_null_cell_not_a_panic() {
        let pod = json!({"metadata": {"name": "web-1"}});
        let table = convert_to_table(&pod);
        assert_eq!(table["rows"][0]["cells"], json!(["web-1", Value::Null]));
    }

    #[test]
    fn the_pod_printer_emits_kubectl_status_columns() {
        let pod = json!({
            "metadata": {"name": "web-1"},
            "spec": {"containers": [{"name": "web"}]},
            "status": {
                "phase": "Running",
                "podIPs": [{"ip": "10.42.0.8"}],
                "containerStatuses": [{
                    "name": "web",
                    "ready": true,
                    "restartCount": 2,
                    "state": {"running": {}}
                }],
                "conditions": [{"type": "Ready", "status": "True"}]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "pods", &pod);
        let columns = table["columnDefinitions"].as_array().unwrap();
        assert_eq!(
            columns
                .iter()
                .map(|column| column["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "Name",
                "Ready",
                "Status",
                "Restarts",
                "Age",
                "IP",
                "Node",
                "Nominated Node",
                "Readiness Gates"
            ]
        );
        let cells = table["rows"][0]["cells"].as_array().unwrap();
        assert_eq!(
            &cells[..5],
            &json!(["web-1", "1/1", "Running", "2", "<unknown>"])
                .as_array()
                .unwrap()[..]
        );
        assert_eq!(
            &cells[5..],
            &json!(["10.42.0.8", "<none>", "<none>", "<none>"])
                .as_array()
                .unwrap()[..]
        );
        assert_eq!(table["rows"][0]["object"], pod);
    }

    #[test]
    fn the_pod_printer_reports_initialization_and_waiting_reasons() {
        let pod = json!({
            "metadata": {"name": "web-1"},
            "spec": {
                "initContainers": [{"name": "setup"}],
                "containers": [{"name": "web"}]
            },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "setup",
                    "restartCount": 1,
                    "state": {"waiting": {"reason": "CrashLoopBackOff"}}
                }]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "pods", &pod);
        assert_eq!(table["rows"][0]["cells"][1], "0/1");
        assert_eq!(table["rows"][0]["cells"][2], "Init:CrashLoopBackOff");
        assert_eq!(table["rows"][0]["cells"][3], "1");
    }

    #[test]
    fn unknown_resources_keep_the_generic_printer() {
        let object = json!({"metadata": {"name": "x"}});
        let table = convert_to_table_for_resource("example.com", "v1", "pods", &object);
        assert_eq!(table["columnDefinitions"].as_array().unwrap().len(), 2);
        let table = convert_to_table_for_resource("", "v1", "configmaps", &object);
        assert_eq!(table["columnDefinitions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn crd_printer_columns_are_evaluated_and_keep_the_declared_types() {
        let columns = json!([
            {"name": "Spec", "type": "string", "description": "The schedule.", "jsonPath": ".spec.schedule"},
            {"name": "Replicas", "type": "integer", "jsonPath": ".status.replicas"},
            {"name": "Ready", "type": "boolean", "jsonPath": ".status.ready"},
            {"name": "Missing", "type": "string", "jsonPath": ".status.missing"}
        ]);
        let object = json!({
            "apiVersion": "example.com/v1",
            "kind": "CronTab",
            "metadata": {"name": "nightly", "creationTimestamp": "2026-08-29T00:00:00Z"},
            "spec": {"schedule": "0 0 * * *"},
            "status": {"replicas": 3, "ready": true}
        });
        let table = convert_to_table_for_resource_with_crd_columns(
            "example.com",
            "v1",
            "crontabs",
            Some(columns.as_array().unwrap()),
            &object,
        );
        assert_eq!(
            table["columnDefinitions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|column| column["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Name", "Spec", "Replicas", "Ready", "Missing"]
        );
        assert_eq!(
            table["columnDefinitions"][1]["description"],
            "The schedule."
        );
        assert_eq!(
            table["rows"][0]["cells"],
            json!(["nightly", "0 0 * * *", 3, true, null])
        );
        assert_eq!(table["rows"][0]["object"], object);
    }

    #[test]
    fn crds_without_additional_printer_columns_get_the_default_age_column() {
        let object = json!({
            "metadata": {"name": "widget", "creationTimestamp": "2026-08-29T00:00:00Z"}
        });
        let columns: Vec<Value> = Vec::new();
        let table = convert_to_table_for_resource_with_crd_columns(
            "example.com",
            "v1",
            "widgets",
            Some(&columns),
            &object,
        );
        assert_eq!(
            table["columnDefinitions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|column| column["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Name", "Age"]
        );
        assert_eq!(
            table["rows"][0]["cells"],
            json!(["widget", "2026-08-29T00:00:00Z"])
        );
    }

    #[test]
    fn common_workload_printers_emit_kubectl_rows() {
        let template = json!({
            "spec": {
                "containers": [
                    {"name": "web", "image": "nginx:1.27"},
                    {"name": "sidecar", "image": "busybox:1.36"}
                ]
            }
        });
        let deployment = json!({
            "metadata": {"name": "web"},
            "spec": {
                "replicas": 3,
                "selector": {"matchLabels": {"app": "web"}},
                "template": template.clone()
            },
            "status": {"readyReplicas": 2, "updatedReplicas": 2, "availableReplicas": 2}
        });
        let table = convert_to_table_for_resource("apps", "v1", "deployments", &deployment);
        assert_eq!(
            table["rows"][0]["cells"],
            json!([
                "web",
                "2/3",
                2,
                2,
                "<unknown>",
                "web,sidecar",
                "nginx:1.27,busybox:1.36",
                "app=web"
            ])
        );

        let replica_set = json!({
            "metadata": {"name": "web-abc"},
            "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "web"}}, "template": template.clone()},
            "status": {"replicas": 2, "readyReplicas": 1}
        });
        let table = convert_to_table_for_resource("apps", "v1", "replicasets", &replica_set);
        assert_eq!(
            &table["rows"][0]["cells"].as_array().unwrap()[1..4],
            json!([3, 2, 1]).as_array().unwrap()
        );

        let stateful_set = json!({
            "metadata": {"name": "web"},
            "spec": {"replicas": 2, "template": template.clone()},
            "status": {"readyReplicas": 1}
        });
        let table = convert_to_table_for_resource("apps", "v1", "statefulsets", &stateful_set);
        assert_eq!(
            &table["rows"][0]["cells"].as_array().unwrap()[..2],
            json!(["web", "1/2"]).as_array().unwrap()
        );

        let daemon_set = json!({
            "metadata": {"name": "agent"},
            "spec": {"selector": {"matchLabels": {"app": "agent"}}, "template": {
                "spec": {"nodeSelector": {"kubernetes.io/os": "linux"}, "containers": [{"name": "agent", "image": "agent:v1"}]}
            }},
            "status": {
                "desiredNumberScheduled": 3,
                "currentNumberScheduled": 3,
                "numberReady": 2,
                "updatedNumberScheduled": 3,
                "numberAvailable": 2
            }
        });
        let table = convert_to_table_for_resource("apps", "v1", "daemonsets", &daemon_set);
        assert_eq!(
            &table["rows"][0]["cells"].as_array().unwrap()[1..8],
            json!([3, 3, 2, 3, 2, "kubernetes.io/os=linux", "<unknown>"])
                .as_array()
                .unwrap()
        );
    }

    #[test]
    fn service_node_and_namespace_printers_emit_kubectl_rows() {
        let service = json!({
            "metadata": {"name": "api"},
            "spec": {
                "type": "LoadBalancer",
                "clusterIP": "10.43.0.10",
                "externalIPs": ["192.0.2.10"],
                "ports": [
                    {"port": 80, "nodePort": 30080, "protocol": "TCP"},
                    {"port": 443, "protocol": "TCP"}
                ]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "services", &service);
        assert_eq!(
            table["rows"][0]["cells"],
            json!([
                "api",
                "LoadBalancer",
                "10.43.0.10",
                "192.0.2.10",
                "80:30080/TCP,443/TCP",
                "<unknown>"
            ])
        );

        let node = json!({
            "metadata": {
                "name": "worker-1",
                "labels": {"node-role.kubernetes.io/worker": ""}
            },
            "spec": {"unschedulable": true},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "addresses": [
                    {"type": "InternalIP", "address": "192.0.2.20"},
                    {"type": "ExternalIP", "address": "198.51.100.20"}
                ],
                "nodeInfo": {
                    "kubeletVersion": "v1.33.0",
                    "osImage": "Debian GNU/Linux",
                    "kernelVersion": "6.1.0",
                    "containerRuntimeVersion": "containerd://2.0.0"
                }
            }
        });
        let table = convert_to_table_for_resource("", "v1", "nodes", &node);
        assert_eq!(
            table["rows"][0]["cells"],
            json!([
                "worker-1",
                "Ready,SchedulingDisabled",
                "worker",
                "<unknown>",
                "v1.33.0",
                "192.0.2.20",
                "198.51.100.20",
                "Debian GNU/Linux",
                "6.1.0",
                "containerd://2.0.0"
            ])
        );

        let namespace = json!({"metadata": {"name": "apps"}, "status": {"phase": "Active"}});
        let table = convert_to_table_for_resource("", "v1", "namespaces", &namespace);
        assert_eq!(
            table["rows"][0]["cells"],
            json!(["apps", "Active", "<unknown>"])
        );
    }
}
