
/// The pure half of [`log_audit_event`] — everything up to the built
/// `Value`, factored out so it's unit-testable without capturing
/// `tracing`'s own log output.
fn build_audit_event(method: &str, path_str: &str, query: &str, user_agent: Option<&str>, identity: Option<&crate::authn::x509::Identity>, peer: &SocketAddr, status: u16, annotations: &BTreeMap<String, String>) -> serde_json::Value {
    include!("body-64-1.rs");
    include!("body-64-2.rs");
}
