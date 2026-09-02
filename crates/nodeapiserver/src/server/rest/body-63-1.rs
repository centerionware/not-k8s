    /// The object as written, `metadata.managedFields` rebuilt to
    /// reflect this apply.
    Applied(Value),
    /// The merged-and-pruned result was byte-for-byte identical to the
    /// object already stored — nothing written, real upstream's own
    /// no-op contract (`crate::patch::updater::Applied::object`'s own
    /// doc comment). The caller still gets a real `200` with the
    /// current object, matching real upstream's own behavior.
    NoOp(Value),
    UnknownResource,
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Validated CRDs normally carry the latter;
