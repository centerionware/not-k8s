    /// converted objects must also satisfy this schema before persistence.
    storage_open_api_schema: Option<Value>,
    /// Only ever meaningfully `true` for a CRD (`schema: None`) whose
    /// matched version declares `subresources.status` — always `true`
    /// for a static built-in, since this crate doesn't model per-type
    /// subresource declarations for built-ins at all yet (a real,
    /// separate, wider gap this field doesn't attempt to close — see
    /// `update_status`/`patch_status`'s own doc comment).
    has_status_subresource: bool,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
