
/// Request headers this build never forwards to an aggregated backend —
/// hop-by-hop headers (`Connection`'s own listed value plus the fixed
/// standard set, RFC 7230 §6.1) and `Host` (rebuilt from the resolved
/// target instead, same as `proxy::http_client::fetch`'s own posture for
/// nodelet).
const HOP_BY_HOP_HEADERS: &[&str] = &["host", "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"];
