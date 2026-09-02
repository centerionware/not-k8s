
/// Implements the core Pod `binding` subresource used by the scheduler.
/// Real upstream's `BindingREST` validates the binding preconditions, sets
/// `spec.nodeName`, merges binding metadata, and marks the Pod scheduled in
/// one optimistic-concurrency write. Keeping that operation separate from
/// generic `update` matters: a Binding is a small request containing only a
/// target, not a replacement Pod object.
pub async fn bind_pod(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
) -> Result<BindOutcome, Error> {
    include!("body-36-1.rs");
    include!("body-36-2.rs");
}
