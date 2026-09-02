
const AGGREGATED_DISCOVERY_CONTENT_TYPE: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";

fn discovery_content_type(parts: &[String], accept_header: Option<&str>) -> &'static str {
    if parts.len() == 1 && matches!(parts.first().map(String::as_str), Some("api") | Some("apis")) && wants_aggregated_discovery(accept_header) {
        AGGREGATED_DISCOVERY_CONTENT_TYPE
    } else {
        "application/json"
    }
}
