
fn log_audit_event(method: &str, path_str: &str, query: &str, user_agent: Option<&str>, identity: Option<&crate::authn::x509::Identity>, peer: &SocketAddr, status: u16, audit_sink: Option<&crate::audit::sink::AuditSink>, annotations: &BTreeMap<String, String>) {
    include!("body-63-1.rs");
    include!("body-63-2.rs");
}
