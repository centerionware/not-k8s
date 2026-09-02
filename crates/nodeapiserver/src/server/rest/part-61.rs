
/// Second-precision RFC3339 with a `Z` suffix (`"2026-08-20T09:30:00Z"`)
/// — matches real upstream's own `metav1.Time` marshaling, which never
/// carries sub-second precision.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    /// The object as it was immediately before deletion — real upstream's
    /// own synchronous-delete response shape (not a bare `Status`, unless
    /// the caller specifically asked for one, which this build doesn't
    /// yet distinguish).
    Deleted(Value),
    UnknownResource,
    ObjectNotFound,
    /// The requested `resourceVersion` or `uid` did not match the live
    /// object. Kubernetes reports this as a conflict and leaves it intact.
    PreconditionFailed,
}

/// Deletes a single object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`list`]/[`create`].
pub async fn delete(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str) -> Result<DeleteOutcome, Error> {
    delete_with_options(storage, group, version, resource, namespace, name, None, false).await
}

/// The subset of Kubernetes `DeleteOptions.preconditions` that can be
/// enforced against nodestore's MVCC-backed objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePreconditions {
    pub resource_version: Option<String>,
    pub uid: Option<String>,
}
