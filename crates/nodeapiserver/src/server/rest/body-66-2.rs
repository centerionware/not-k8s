    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
    /// The merged-and-pruned result was identical to what's already
    /// stored (or, for create-on-apply, `config` was itself empty) —
    /// nothing to persist, `Value` is what to return to the caller.
    NoOp(Value),
