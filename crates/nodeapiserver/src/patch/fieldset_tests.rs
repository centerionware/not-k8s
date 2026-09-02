#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_serializes_with_the_real_f_prefix() {
        assert_eq!(
            serialize_path_element(&PathElement::Field("spec".to_string())),
            "f:spec"
        );
    }

    #[test]
    fn index_serializes_with_the_real_i_prefix() {
        assert_eq!(serialize_path_element(&PathElement::Index(3)), "i:3");
    }

    #[test]
    fn value_serializes_its_own_json_after_the_v_prefix() {
        assert_eq!(
            serialize_path_element(&PathElement::Value(json!("foo"))),
            "v:\"foo\""
        );
    }

    #[test]
    fn key_serializes_as_a_compact_json_object_after_the_k_prefix() {
        let pe = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert_eq!(serialize_path_element(&pe), "k:{\"name\":\"nginx\"}");
    }

    #[test]
    fn deserialize_round_trips_every_real_variant() {
        for pe in [
            PathElement::Field("spec".to_string()),
            PathElement::Index(3),
            PathElement::Value(json!(true)),
            PathElement::Key(vec![("name".to_string(), json!("nginx"))]),
        ] {
            let s = serialize_path_element(&pe);
            assert_eq!(
                deserialize_path_element(&s).unwrap(),
                pe,
                "round trip failed for {s:?}"
            );
        }
    }

    #[test]
    fn an_unknown_type_prefix_is_a_named_error_at_the_element_level() {
        assert!(matches!(
            deserialize_path_element("x:whatever"),
            Err(DeserializeError::UnknownType(_))
        ));
    }

    #[test]
    fn a_leaf_member_with_no_children_encodes_as_an_empty_object() {
        let mut set = Set::new();
        set.insert(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("replicas".to_string()),
        ]);
        let doc = set.to_json();
        assert_eq!(doc, json!({"f:spec": {"f:replicas": {}}}));
    }

    #[test]
    fn a_member_that_also_has_children_gets_the_real_dot_marker() {
        // metadata.labels is itself owned (the whole map was set) AND
        // metadata.labels.app is separately tracked as a child -- the
        // one real case upstream's own "." marker exists for.
        let mut set = Set::new();
        set.insert(&[
            PathElement::Field("metadata".to_string()),
            PathElement::Field("labels".to_string()),
        ]);
        set.insert(&[
            PathElement::Field("metadata".to_string()),
            PathElement::Field("labels".to_string()),
            PathElement::Field("app".to_string()),
        ]);
        let doc = set.to_json();
        assert_eq!(
            doc,
            json!({"f:metadata": {"f:labels": {".": {}, "f:app": {}}}})
        );
    }

    #[test]
    fn a_real_fieldsv1_document_round_trips_through_from_json_and_to_json() {
        let doc = json!({
            "f:metadata": {
                "f:labels": {".": {}, "f:app": {}},
            },
            "f:spec": {
                "f:replicas": {},
                "f:containers": {"k:{\"name\":\"nginx\"}": {"f:image": {}}},
            },
        });
        let set = Set::from_json(&doc).unwrap();
        assert!(set.has(&[
            PathElement::Field("metadata".to_string()),
            PathElement::Field("labels".to_string())
        ]));
        assert!(set.has(&[
            PathElement::Field("metadata".to_string()),
            PathElement::Field("labels".to_string()),
            PathElement::Field("app".to_string())
        ]));
        assert!(set.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("replicas".to_string())
        ]));
        assert!(set.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("containers".to_string()),
            PathElement::Key(vec![("name".to_string(), json!("nginx"))]),
            PathElement::Field("image".to_string())
        ]));
        assert_eq!(
            set.to_json(),
            doc,
            "a real fieldsV1 document must round trip byte-for-byte (modulo JSON key order)"
        );
    }

    #[test]
    fn has_is_false_for_a_path_never_inserted() {
        let mut set = Set::new();
        set.insert(&[PathElement::Field("spec".to_string())]);
        assert!(!set.has(&[PathElement::Field("status".to_string())]));
        assert!(!set.has(&[
            PathElement::Field("spec".to_string()),
            PathElement::Field("replicas".to_string())
        ]));
    }

    #[test]
    fn an_empty_path_is_never_a_member() {
        let set = Set::new();
        assert!(!set.has(&[]));
    }

    // `set_from_object`'s own tests, each driven by a real vendored field
    // confirmed directly against `vendor/openapi-spec/v3` before writing
    // the code, not assumed from the SMD spec alone -- see this module's
    // own doc comment on `set_from_object` for the exact confirmation.

    #[test]
    fn a_list_type_map_field_tracks_each_element_by_its_own_key_and_recurses_into_it() {
        // PodSpec.containers: x-kubernetes-list-type: map, list-map-keys:
        // [name], element schema Container.
        let pod_spec = json!({"containers": [{"name": "nginx", "image": "nginx:latest"}]});
        let set = set_from_object("io.k8s.api.core.v1.PodSpec", &pod_spec);
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert!(
            set.has(&[
                PathElement::Field("containers".to_string()),
                key.clone(),
                PathElement::Field("name".to_string())
            ]),
            "the key field itself must also be tracked as a child, matching real fieldsV1 documents"
        );
        assert!(set.has(&[
            PathElement::Field("containers".to_string()),
            key,
            PathElement::Field("image".to_string())
        ]));
    }

    #[test]
    fn a_list_type_set_field_tracks_each_element_as_its_own_value_leaf_with_no_recursion() {
        // ObjectMeta.finalizers: x-kubernetes-list-type: set, scalar elements.
        let meta = json!({"finalizers": ["a.example.com/finalizer", "b.example.com/finalizer"]});
        let set = set_from_object("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &meta);
        assert!(set.has(&[
            PathElement::Field("finalizers".to_string()),
            PathElement::Value(json!("a.example.com/finalizer"))
        ]));
        assert!(set.has(&[
            PathElement::Field("finalizers".to_string()),
            PathElement::Value(json!("b.example.com/finalizer"))
        ]));
    }

    #[test]
    fn a_list_type_atomic_field_is_one_leaf_for_the_whole_list_not_per_element() {
        // Container.command: x-kubernetes-list-type: atomic (explicit).
        let container = json!({"command": ["/bin/sh", "-c", "echo hi"]});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(
            set.has(&[PathElement::Field("command".to_string())]),
            "the whole list must be tracked as one leaf"
        );
        assert!(
            !set.children
                .contains_key(&PathElement::Field("command".to_string())),
            "an atomic list must have no per-element children at all"
        );
    }

    #[test]
    fn a_map_type_atomic_field_is_one_leaf_for_the_whole_map_not_per_key() {
        // PodSpec.nodeSelector: x-kubernetes-map-type: atomic.
        let pod_spec = json!({"nodeSelector": {"disktype": "ssd", "region": "us-west"}});
        let set = set_from_object("io.k8s.api.core.v1.PodSpec", &pod_spec);
        assert!(
            set.has(&[PathElement::Field("nodeSelector".to_string())]),
            "the whole map must be tracked as one leaf"
        );
        assert!(
            !set.children
                .contains_key(&PathElement::Field("nodeSelector".to_string())),
            "an atomic map must have no per-key children at all"
        );
    }

    #[test]
    fn a_generic_map_with_no_known_schema_tracks_each_key_separately() {
        // ObjectMeta.labels carries no ref_schema (scalar-valued
        // additionalProperties) -- real upstream's own granular-map
        // default still applies.
        let meta = json!({"labels": {"app": "nginx", "tier": "frontend"}});
        let set = set_from_object("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &meta);
        assert!(set.has(&[
            PathElement::Field("labels".to_string()),
            PathElement::Field("app".to_string())
        ]));
        assert!(set.has(&[
            PathElement::Field("labels".to_string()),
            PathElement::Field("tier".to_string())
        ]));
        assert!(
            !set.has(&[PathElement::Field("labels".to_string())]),
            "the map field itself must not be a leaf member -- only its individual keys are"
        );
    }

    #[test]
    fn a_scalar_field_is_always_a_leaf() {
        let container = json!({"name": "nginx", "image": "nginx:latest"});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(set.has(&[PathElement::Field("name".to_string())]));
        assert!(set.has(&[PathElement::Field("image".to_string())]));
    }

    #[test]
    fn a_nested_struct_field_recurses_using_its_own_ref_schema() {
        // Container.resources -> ResourceRequirements -> .limits (a map
        // of Quantity, itself a real nested-schema recursion chain).
        let container = json!({"name": "nginx", "resources": {"limits": {"cpu": "500m"}}});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(
            set.has(&[
                PathElement::Field("resources".to_string()),
                PathElement::Field("limits".to_string()),
                PathElement::Field("cpu".to_string())
            ]) || set.has(&[
                PathElement::Field("resources".to_string()),
                PathElement::Field("limits".to_string())
            ]),
            "resources.limits.cpu must be tracked one way or the other depending on whether ResourceRequirements.limits itself carries ref_schema metadata"
        );
    }

    // `Set` algebra (`union`/`intersection`/`difference`/
    // `recursive_difference`) -- each built from two small hand-built
    // sets rather than a real object, so the exact tree shape under test
    // is unambiguous.

    fn set_of(paths: &[&[PathElement]]) -> Set {
        let mut s = Set::new();
        for p in paths {
            s.insert(p);
        }
        s
    }

    fn f(name: &str) -> PathElement {
        PathElement::Field(name.to_string())
    }

    #[test]
    fn union_combines_members_from_both_sides() {
        let a = set_of(&[&[f("spec"), f("replicas")]]);
        let b = set_of(&[&[f("spec"), f("selector")]]);
        let u = a.union(&b);
        assert!(u.has(&[f("spec"), f("replicas")]));
        assert!(u.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn union_merges_a_shared_child_node_rather_than_overwriting_it() {
        let a = set_of(&[&[f("metadata"), f("labels"), f("app")]]);
        let b = set_of(&[&[f("metadata"), f("labels"), f("tier")]]);
        let u = a.union(&b);
        assert!(
            u.has(&[f("metadata"), f("labels"), f("app")]),
            "the union must not lose a's own child under a shared parent"
        );
        assert!(u.has(&[f("metadata"), f("labels"), f("tier")]));
    }

    #[test]
    fn intersection_keeps_only_paths_present_on_both_sides() {
        let a = set_of(&[&[f("spec"), f("replicas")], &[f("spec"), f("selector")]]);
        let b = set_of(&[&[f("spec"), f("replicas")]]);
        let i = a.intersection(&b);
        assert!(i.has(&[f("spec"), f("replicas")]));
        assert!(!i.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn intersection_of_disjoint_sets_is_empty() {
        let a = set_of(&[&[f("spec"), f("replicas")]]);
        let b = set_of(&[&[f("status"), f("readyReplicas")]]);
        assert!(a.intersection(&b).is_empty());
    }

    #[test]
    fn difference_removes_shared_leaves() {
        let a = set_of(&[&[f("spec"), f("replicas")], &[f("spec"), f("selector")]]);
        let b = set_of(&[&[f("spec"), f("replicas")]]);
        let d = a.difference(&b);
        assert!(!d.has(&[f("spec"), f("replicas")]));
        assert!(d.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn difference_of_a_set_with_itself_is_empty() {
        let a = set_of(&[
            &[f("spec"), f("replicas")],
            &[f("metadata"), f("labels"), f("app")],
        ]);
        assert!(a.difference(&a).is_empty());
    }

    /// The real, intentional asymmetry `difference`'s own doc comment
    /// names: a subtree survives a plain `difference` against a shallow
    /// leaf at the same path in `other`, but not against
    /// `recursive_difference`.
    #[test]
    fn plain_difference_does_not_let_an_others_leaf_cancel_a_selfs_subtree() {
        let a = set_of(&[&[f("a"), f("b"), f("c")]]);
        let b = set_of(&[&[f("a")]]); // "a" owned as a shallow leaf, not a subtree
        let d = a.difference(&b);
        assert!(
            d.has(&[f("a"), f("b"), f("c")]),
            "difference must leave self's own deeper subtree alone here"
        );
    }

    #[test]
    fn recursive_difference_drops_a_whole_subtree_when_others_leaf_matches_its_root() {
        let a = set_of(&[&[f("a"), f("b"), f("c")]]);
        let b = set_of(&[&[f("a"), f("b")]]);
        let d = a.recursive_difference(&b);
        assert!(
            !d.has(&[f("a"), f("b"), f("c")]),
            "the entire a.b subtree must be gone"
        );
    }

    #[test]
    fn is_empty_is_true_for_a_freshly_constructed_set() {
        assert!(Set::new().is_empty());
    }

    #[test]
    fn is_empty_is_false_once_anything_is_inserted() {
        let s = set_of(&[&[f("spec")]]);
        assert!(!s.is_empty());
    }

    // `remove_items` -- real `TypedValue.RemoveItems`, removal mode only.

    #[test]
    fn remove_items_drops_an_exactly_named_scalar_field() {
        let value = json!({"replicas": 3, "minReadySeconds": 10});
        let to_remove = set_of(&[&[f("replicas")]]);
        let result = remove_items("io.k8s.api.apps.v1.DeploymentSpec", &value, &to_remove);
        assert_eq!(result, json!({"minReadySeconds": 10}));
    }

    #[test]
    fn remove_items_leaves_fields_it_was_not_told_to_touch_alone() {
        let value = json!({"replicas": 3, "minReadySeconds": 10});
        let to_remove = set_of(&[&[f("selector")]]); // names a field this object doesn't even have
        let result = remove_items("io.k8s.api.apps.v1.DeploymentSpec", &value, &to_remove);
        assert_eq!(result, value);
    }

    #[test]
    fn remove_items_drops_a_whole_associative_list_element_by_its_key() {
        let value = json!({"containers": [
            {"name": "nginx", "image": "nginx:latest"},
            {"name": "sidecar", "image": "busybox"},
        ]});
        let key = PathElement::Key(vec![("name".to_string(), json!("sidecar"))]);
        let to_remove = set_of(&[&[f("containers"), key]]);
        let result = remove_items("io.k8s.api.core.v1.PodSpec", &value, &to_remove);
        assert_eq!(
            result,
            json!({"containers": [{"name": "nginx", "image": "nginx:latest"}]})
        );
    }

    #[test]
    fn remove_items_removes_one_field_within_an_associative_list_element_leaving_the_rest() {
        let value = json!({"containers": [
            {"name": "nginx", "image": "nginx:latest", "command": ["/bin/sh"]},
        ]});
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        let to_remove = set_of(&[&[f("containers"), key, f("command")]]);
        let result = remove_items("io.k8s.api.core.v1.PodSpec", &value, &to_remove);
        assert_eq!(
            result,
            json!({"containers": [{"name": "nginx", "image": "nginx:latest"}]})
        );
    }

    #[test]
    fn remove_items_removes_one_key_from_a_generic_map_field_leaving_the_field_and_the_rest() {
        let value = json!({"labels": {"app": "nginx", "tier": "frontend"}});
        let to_remove = set_of(&[&[f("labels"), f("app")]]);
        let result = remove_items(
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta",
            &value,
            &to_remove,
        );
        assert_eq!(result, json!({"labels": {"tier": "frontend"}}));
    }

    #[test]
    fn remove_items_removing_every_key_of_a_generic_map_preserves_it_as_an_empty_object() {
        let value = json!({"labels": {"app": "nginx"}});
        let to_remove = set_of(&[&[f("labels"), f("app")]]);
        let result = remove_items(
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta",
            &value,
            &to_remove,
        );
        assert_eq!(
            result,
            json!({"labels": {}}),
            "the field itself wasn't exactly matched, only a child of it -- must survive as {{}}, not vanish or become null"
        );
    }

    #[test]
    fn remove_items_drops_a_set_typed_list_element_by_value() {
        let value = json!({"finalizers": ["a.example.com/f", "b.example.com/f"]});
        let to_remove = set_of(&[&[
            f("finalizers"),
            PathElement::Value(json!("a.example.com/f")),
        ]]);
        let result = remove_items(
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta",
            &value,
            &to_remove,
        );
        assert_eq!(result, json!({"finalizers": ["b.example.com/f"]}));
    }

    #[test]
    fn remove_items_exactly_naming_the_whole_field_drops_it_even_with_no_further_children() {
        let value = json!({"nodeSelector": {"disktype": "ssd"}, "restartPolicy": "Always"});
        let to_remove = set_of(&[&[f("nodeSelector")]]);
        let result = remove_items("io.k8s.api.core.v1.PodSpec", &value, &to_remove);
        assert_eq!(result, json!({"restartPolicy": "Always"}));
    }

    // `ensure_named_fields_are_members`

    #[test]
    fn promotes_a_named_struct_field_that_only_has_leaf_children() {
        // Only "labels.app" was ever tracked as a member -- "labels"
        // itself, a real declared ObjectMeta field, must be promoted too.
        let set = set_of(&[&[f("labels"), f("app")]]);
        let promoted = ensure_named_fields_are_members(
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta",
            &set,
        );
        assert!(
            promoted.members.contains(&f("labels")),
            "labels is a real ObjectMeta field, must be promoted"
        );
        assert!(
            promoted.has(&[f("labels"), f("app")]),
            "the original leaf must survive the promotion"
        );
    }

    #[test]
    fn promotes_a_named_list_field_but_never_the_associative_key_itself() {
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        let set = set_of(&[&[f("containers"), key.clone(), f("image")]]);
        let promoted = ensure_named_fields_are_members("io.k8s.api.core.v1.PodSpec", &set);
        assert!(
            promoted.members.contains(&f("containers")),
            "containers is a real PodSpec field, must be promoted"
        );
        let containers_children = &promoted.children[&f("containers")];
        assert!(
            !containers_children.members.contains(&key),
            "a Key path element is never promoted to a member"
        );
        assert!(
            containers_children.has(&[key, f("image")]),
            "the original leaf must survive"
        );
    }

    #[test]
    fn already_present_members_are_not_duplicated() {
        let mut set = set_of(&[&[f("labels")], &[f("labels"), f("app")]]);
        set.insert(&[f("labels")]); // already a member (the "." marker case)
        let promoted = ensure_named_fields_are_members(
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta",
            &set,
        );
        assert_eq!(
            promoted
                .members
                .iter()
                .filter(|pe| **pe == f("labels"))
                .count(),
            1,
            "must not duplicate an already-present member"
        );
    }

    #[test]
    fn a_set_with_no_children_at_all_is_unchanged() {
        let set = set_of(&[&[f("replicas")]]);
        let promoted = ensure_named_fields_are_members("io.k8s.api.apps.v1.DeploymentSpec", &set);
        assert_eq!(promoted, set);
    }
}
