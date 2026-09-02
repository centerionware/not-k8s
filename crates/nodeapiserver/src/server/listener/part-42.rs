
/// Returns the path after the `proxy` marker.  `RequestInfo.parts` has
/// already removed the API prefix, group/version, and optional namespace,
/// so this handles both supported Kubernetes forms:
/// `.../{resource}/{name}/proxy/{path}` and
/// `.../proxy/{resource}/{name}/{path}`.
fn proxy_suffix(info: &path::RequestInfo) -> String {
    include!("body-70-1.rs");
    include!("body-70-2.rs");
}
