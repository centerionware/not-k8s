
/// The core Pod `ephemeralcontainers` subresource only exposes the Pod
/// object; its strategy permits changing `spec.ephemeralContainers` and
/// resets every other attempted change back to the stored Pod. Existing
/// ephemeral containers are immutable, so a caller may only append valid
/// new entries. This is the same boundary enforced by upstream's
/// `EphemeralContainersStrategy` before its normal optimistic-concurrency
/// store update.
fn restrict_ephemeral_container_update(existing: &Value, candidate: &Value) -> Result<Value, Vec<String>> {
    include!("body-37-1.rs");
    include!("body-37-2.rs");
}
