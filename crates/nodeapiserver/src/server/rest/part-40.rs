
/// Reads the current object and applies one of this build's three real
/// patch kinds to it — the "prepare" half of [`patch`], split out so a
/// caller (`server::listener`) can run Group J admission against the
/// real candidate object before committing to [`patch_persist`].
pub async fn patch_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<PatchPrepareOutcome, Error> {
    include!("body-57-1.rs");
    include!("body-57-2.rs");
}
