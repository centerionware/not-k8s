    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
    /// Another manager owns a field this apply is changing — real
    /// upstream's own `409 Conflict`, not raised unless `force` is
    /// false.
    Conflict(Vec<crate::patch::updater::Conflict>),
    Invalid(Vec<String>),
