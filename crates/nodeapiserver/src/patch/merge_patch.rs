//! `application/merge-patch+json` — RFC 7386. A thin named wrapper around
//! the `json-patch` crate, same reasoning as `patch::json_patch`'s own
//! module doc comment.

use serde_json::Value;

/// Applies an RFC 7386 merge patch to `target`, in place. `null` in the
/// patch deletes the corresponding key (RFC 7386 §1); an object merges
/// recursively; anything else replaces wholesale — all `json_patch::merge`'s
/// own behavior, not reimplemented here.
pub fn apply(target: &mut Value, patch_doc: &Value) {
    json_patch::merge(target, patch_doc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_and_new_fields_are_set() {
        let mut target = json!({"a": 1});
        apply(&mut target, &json!({"a": 2, "b": 3}));
        assert_eq!(target, json!({"a": 2, "b": 3}));
    }

    #[test]
    fn null_in_the_patch_deletes_the_field() {
        let mut target = json!({"a": 1, "b": 2});
        apply(&mut target, &json!({"a": null}));
        assert_eq!(target, json!({"b": 2}));
    }

    #[test]
    fn nested_objects_merge_recursively_rather_than_being_replaced_wholesale() {
        let mut target = json!({"outer": {"a": 1, "b": 2}});
        apply(&mut target, &json!({"outer": {"a": 10}}));
        assert_eq!(target, json!({"outer": {"a": 10, "b": 2}}), "b must survive — RFC 7386 merges objects, not replaces them");
    }

    #[test]
    fn an_array_value_in_the_patch_replaces_the_whole_array() {
        // RFC 7386 has no concept of merging arrays by index/key — that is
        // exactly the gap Strategic Merge Patch (patch::strategic_merge)
        // exists to fill for the k8s-specific case.
        let mut target = json!({"list": [1, 2, 3]});
        apply(&mut target, &json!({"list": [9]}));
        assert_eq!(target, json!({"list": [9]}));
    }
}
