pub enum UpdateOutcome {
    Updated(Value),
    UnknownResource,
    /// No object exists at this key — this build doesn't support
    /// create-on-update (`AllowCreateOnUpdate`, real upstream's own
    /// opt-in a handful of types use), named honestly rather than
    /// silently creating one.
    ObjectNotFound,
    /// The submitted body had no `metadata.resourceVersion` at all —
    /// real upstream's own generic registry requires one for `PUT`
    /// (optimistic concurrency has nothing to compare against
    /// otherwise).
    MissingResourceVersion,
    /// The submitted `resourceVersion` didn't match what's currently
    /// stored — a real conflict, matching real upstream's own
    /// `errors.NewConflict`.
    Conflict,
    NamespaceMismatch,
    Invalid(Vec<String>),
    /// [`patch`] only: the `Content-Type` wasn't one of the three real
    /// patch media types this build understands.
    UnsupportedPatchType,
}

/// The virtual `autoscaling/v1 Scale` resource exposed by the built-in
/// workload scale subresources. Scale reads and writes are translated to
/// the parent object's `spec.replicas`; the Scale object itself is never
/// persisted in nodestore.
#[derive(Debug, PartialEq)]
