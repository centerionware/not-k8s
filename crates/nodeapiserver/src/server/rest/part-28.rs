
fn ephemeral_container_list(object: &Value) -> Result<&[Value], Vec<String>> {
    match object.pointer("/spec/ephemeralContainers") {
        None => Ok(&[]),
        Some(Value::Array(containers)) => Ok(containers),
        Some(_) => Err(vec!["spec.ephemeralContainers: must be an array".to_string()]),
    }
}

/// Reads a Pod through the `ephemeralcontainers` subresource. Upstream
/// returns the complete Pod because the subresource strategy only narrows
/// writes; the caller still needs the ordinary metadata and status fields
/// to observe the result.
pub async fn get_ephemeral_containers(storage: &mut StorageClient, namespace: &str, name: &str) -> Result<GetOutcome, Error> {
    get(storage, None, "", "v1", "pods", Some(namespace), name).await
}
