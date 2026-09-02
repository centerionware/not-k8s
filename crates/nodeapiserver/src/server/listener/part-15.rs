
/// Real upstream's own Server-Side Apply media type
/// (`application/apply-patch+yaml`) — the one `rest::
/// patch_kind_for_content_type` deliberately doesn't recognize (its own
/// doc comment), since it isn't one of that function's three patch
/// kinds; this is the separate check that routes a `PATCH` into
/// `rest::server_side_apply` instead.
fn is_apply_patch_content_type(content_type: &str) -> bool {
    content_type.split(';').next().unwrap_or("").trim() == "application/apply-patch+yaml"
}

/// Real upstream's own required `?fieldManager=` query parameter for
/// Server-Side Apply — `path::RequestInfo` doesn't carry it, same reason
/// `resource_version_query` above doesn't come from there either.
/// `None` when absent, so the caller can reject with a real `400` rather
/// than inventing a manager name.
fn field_manager_query(query: &str) -> Option<String> {
    path::parse_query(query).into_iter().find(|(k, _)| k == "fieldManager").map(|(_, v)| v).filter(|value| !value.is_empty())
}

/// Real upstream's own `?force=` query parameter — Server-Side Apply's
/// conflict-override flag.
fn force_query(query: &str) -> bool {
    path::parse_query(query).iter().any(|(k, v)| k == "force" && v == "true")
}
