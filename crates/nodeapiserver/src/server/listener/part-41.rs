
fn is_proxy_request(info: &path::RequestInfo) -> bool {
    info.verb == "proxy" || info.subresource == "proxy"
}
