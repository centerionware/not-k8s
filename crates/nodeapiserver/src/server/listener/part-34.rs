
fn is_mutating_request(info: &path::RequestInfo) -> bool {
    matches!(
        info.verb.as_str(),
        "create" | "update" | "patch" | "delete" | "deletecollection"
    )
}
