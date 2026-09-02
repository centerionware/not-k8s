
/// Decodes a Kubernetes protobuf request envelope after resolving the
/// resource named by the URL. Built-in resources use their generated schema;
/// CRD objects use the envelope's raw JSON body because Kubernetes does not
/// generate a compiled protobuf schema for operator-defined kinds.
pub async fn decode_protobuf_request(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    bytes: &[u8],
) -> Result<Option<Value>, Error> {
    include!("body-19-1.rs");
    include!("body-19-2.rs");
}
