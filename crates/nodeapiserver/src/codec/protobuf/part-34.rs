
/// Wraps an already-encoded object body in the full
/// `application/vnd.kubernetes.protobuf` wire payload: the 4-byte `k8s\0`
/// magic, then a length-delimited (implicit — this is the top-level
/// message, so no outer tag) `runtime.Unknown` whose `raw` field holds
/// `object_bytes` and whose `typeMeta` names `api_version`/`kind`.
pub fn wrap_unknown(api_version: &str, kind: &str, object_bytes: &[u8]) -> Vec<u8> {
    include!("body-42-1.rs");
    include!("body-42-2.rs");
}
