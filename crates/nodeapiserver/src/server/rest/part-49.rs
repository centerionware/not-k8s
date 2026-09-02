
/// The "prepare" half of [`server_side_apply`]: resolves the resource,
/// reads the current object (if any), runs the real `updater::apply`
/// orchestration, rebuilds `managedFields`, and validates/defaults the
/// result — everything short of the actual `Txn` write, so a caller can
/// run Group J admission against the real candidate object in between
/// (`server::listener`'s own `PATCH` branch does exactly this for
/// `LimitRanger`, mirroring how [`patch_prepare`]/[`patch_persist`]
/// already split for the same reason).
pub async fn apply_prepare(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyPrepareOutcome, Error> {
    include!("body-67-1.rs");
    include!("body-67-2.rs");
}
