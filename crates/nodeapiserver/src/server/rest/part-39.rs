
/// The context [`patch_prepare`] hands back to [`patch_persist`] once a
/// patch has been applied but before it's validated/persisted — enough
/// to run Group J admission against the real candidate object in
/// between (`server::listener`'s own `PATCH` branch does exactly this
/// for `LimitRanger`), without re-fetching or re-applying the patch a
/// second time.
#[derive(Debug)]
pub struct PatchContext {
    /// `None` for a CRD-defined resource — see [`apply_patch`]'s own doc
    /// comment for what that rules out (`strategic-merge-patch`) and
    /// what it doesn't (`JSON Patch`/`Merge Patch`, and
    /// [`patch_persist`]'s own schema-driven defaulting, which falls
    /// back to `open_api_schema` in exactly this case).
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    storage_open_api_schema: Option<Value>,
    kind: String,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    key: String,
    existing_kv: mvccpb::KeyValue,
    existing_object: Value,
}

#[derive(Debug)]
pub enum PatchPrepareOutcome {
    /// The patch applied cleanly; `candidate` is the resulting object,
    /// not yet validated/defaulted/persisted.
    Ready(Value, PatchContext),
    UnknownResource,
    ObjectNotFound,
    /// The patch itself couldn't be applied (a JSON Patch `test` op
    /// failure, or a malformed patch document).
    Invalid(Vec<String>),
}

/// Applies one of this build's three real patch kinds
/// ([`crate::patch::json_patch`]/[`crate::patch::merge_patch`]/
/// [`crate::patch::strategic_merge`], all landed in Group G) to
/// `existing`. Shared by [`patch_prepare`] (patches the whole object)
/// and [`patch_status`] (patches the whole object too — real upstream's
/// own subresource PATCH semantics: the patch document can reference
/// any path, only the final write is restricted to `.status` — the
/// restriction happens at persist time, not by scoping what the patch
/// itself can touch).
///
/// `schema` is `None` for a CRD-defined resource — `JSON Patch`/`Merge
/// Patch` need no schema at all and work identically either way;
/// `strategic-merge-patch` uses `open_api_schema` instead in that case
/// (`apiextensions::schema_strategic_merge`, the runtime-schema sibling
/// of `crate::patch::strategic_merge`'s own compiled-`ref_schema` walk).
/// `open_api_schema` is `None` too only for a CRD version whose own
/// document carries no schema at all (a real, if unusual, case this
/// build's own read path already tolerates elsewhere — a malformed/
/// legacy document, `apiextensions::registry::CrdResource`'s own doc
/// comment) — a `strategic-merge-patch` against one has no schema of any
/// kind to interpret, a real `Invalid`, not a panic.
fn apply_patch(kind_of_patch: PatchKind, schema: Option<&str>, open_api_schema: Option<&Value>, existing: &Value, patch_doc: &Value) -> Result<Value, String> {
    match kind_of_patch {
        PatchKind::Json => {
            let mut object = existing.clone();
            if crate::patch::json_patch::apply(&mut object, patch_doc).is_err() {
                return Err("the submitted JSON Patch could not be applied".to_string());
            }
            Ok(object)
        }
        PatchKind::Merge => {
            let mut object = existing.clone();
            crate::patch::merge_patch::apply(&mut object, patch_doc);
            Ok(object)
        }
        PatchKind::StrategicMerge => match (schema, open_api_schema) {
            (Some(schema), _) => Ok(crate::patch::strategic_merge::apply(schema, existing, patch_doc)),
            (None, Some(open_api_schema)) => Ok(apiextensions::schema_strategic_merge::apply(open_api_schema, existing, patch_doc)),
            (None, None) => Err("strategic-merge-patch: this resource has no known schema to interpret x-kubernetes-list-type/-list-map-keys against".to_string()),
        },
    }
}
