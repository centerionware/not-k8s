    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource.is_empty() {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);

        // Server-Side Apply — its own branch, not folded into the
        // three-patch-kind block below: `rest::patch_kind_for_content_type`
        // deliberately doesn't recognize this media type (its own doc
        // comment), the body is YAML (or JSON, a valid subset), and the
        // real orchestration (`rest::apply_prepare`/`apply_persist`,
        // Group G's `updater::apply` wired to storage) is a wholly
        // different code path from the three-patch-kind `rest::
        // patch_prepare`/`patch_persist` split above -- but the *same
        // shape* of split, for the same reason: so both
        // `namespace_lifecycle` and `LimitRanger` admission can run
        // against the real candidate object in between, matching the
        // three-patch-kind branch's own coverage exactly. **Named,
        // The runtime-schema CRD path is handled by the same orchestration;
        // schema-less legacy CRD records remain a defensive 501 outcome.
include!("patch_apply.rs");
include!("patch_standard.rs");
    }
