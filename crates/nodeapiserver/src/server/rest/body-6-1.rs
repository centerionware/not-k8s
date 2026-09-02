    kind: String,
    /// `Some(proto message name)` for a built-in; `None` for a CRD.
    schema: Option<&'static str>,
    open_api_schema: Option<Value>,
    /// The CRD storage version's schema, when this is a dynamic resource.
    /// Requests are validated against their served version before conversion;
