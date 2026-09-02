    Ready(Value, ApplyContext),
    UnknownResource,
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Established CRDs normally carry the latter;
