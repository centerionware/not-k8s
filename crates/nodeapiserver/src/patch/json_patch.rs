//! `application/json-patch+json` — RFC 6902. A thin named wrapper around
//! the `json-patch` crate (`docs/APISERVER_PLAN.md` finding 8: reused, not
//! hand-written) so `patch::mod`'s dispatch by content type has one
//! function per patch kind regardless of which crate implements it.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parsing JSON Patch document: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("applying JSON Patch: {0}")]
    Apply(#[from] json_patch::PatchError),
}

/// Applies an RFC 6902 patch document (a JSON array of operations) to
/// `target`, in place. Rolls back to `target`'s original state if any
/// operation fails partway through — `json_patch::patch`'s own behavior,
/// not `patch_unsafe`'s (a partially-applied patch is worse than an
/// all-or-nothing failure for an update the apiserver is about to persist).
pub fn apply(target: &mut Value, patch_doc: &Value) -> Result<(), Error> {
    let ops: json_patch::Patch = serde_json::from_value(patch_doc.clone())?;
    json_patch::patch(target, &ops)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replace_and_add_operations_apply_in_order() {
        let mut target = json!({"a": 1, "b": {"c": 2}});
        let patch_doc = json!([
            {"op": "replace", "path": "/a", "value": 10},
            {"op": "add", "path": "/b/d", "value": 3},
        ]);
        apply(&mut target, &patch_doc).unwrap();
        assert_eq!(target, json!({"a": 10, "b": {"c": 2, "d": 3}}));
    }

    #[test]
    fn remove_operation_deletes_the_field() {
        let mut target = json!({"a": 1, "b": 2});
        apply(&mut target, &json!([{"op": "remove", "path": "/b"}])).unwrap();
        assert_eq!(target, json!({"a": 1}));
    }

    #[test]
    fn a_failing_operation_rolls_back_every_earlier_operation_in_the_same_patch() {
        let mut target = json!({"a": 1});
        let patch_doc = json!([
            {"op": "replace", "path": "/a", "value": 99},
            {"op": "remove", "path": "/nonexistent"},
        ]);
        let err = apply(&mut target, &patch_doc).expect_err("the second op should fail");
        assert!(matches!(err, Error::Apply(_)));
        assert_eq!(target, json!({"a": 1}), "the first op's effect must be rolled back, not left applied");
    }

    #[test]
    fn a_test_operation_that_fails_rejects_the_whole_patch() {
        let mut target = json!({"a": 1});
        let err = apply(&mut target, &json!([{"op": "test", "path": "/a", "value": 2}])).expect_err("test op should fail: a is 1, not 2");
        assert!(matches!(err, Error::Apply(_)));
    }
}
