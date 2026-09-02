macro_rules! handle_patch {
    (
        $req:ident, $storage:ident, $cache_registry:ident,
        $pure_admission:ident, $pod_node_selector_config:ident,
        $identity:ident, $service_account_authenticator:ident,
        $enforce_rbac:ident, $authorization_webhook_allowed:ident,
        $aggregation_proxy_identity:ident, $kubelet_tls:ident,
        $method:ident, $path_str:ident, $query:ident, $info:ident,
        $request_field_manager:ident, $admission_metadata:ident,
        $is_get:ident, $is_list:ident, $is_create:ident,
        $is_delete:ident, $is_update:ident, $is_watch:ident,
        $is_certificate_status_subresource:ident,
        $wants_partial_metadata:ident, $has_body:ident
    ) => {{
        if $info.is_resource_request && $info.verb == "patch" && !$info.name.is_empty() && $info.subresource.is_empty() {
            let content_type = $req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
            let namespace = storage_namespace(&$info);
            handle_patch_apply!($req, $storage, $cache_registry, $pure_admission, $pod_node_selector_config, $identity, $service_account_authenticator, $enforce_rbac, $authorization_webhook_allowed, $aggregation_proxy_identity, $kubelet_tls, $method, $path_str, $query, $info, $request_field_manager, $admission_metadata, $is_get, $is_list, $is_create, $is_delete, $is_update, $is_watch, $is_certificate_status_subresource, $wants_partial_metadata, $has_body, content_type, namespace);
            handle_patch_standard!($req, $storage, $cache_registry, $pure_admission, $pod_node_selector_config, $identity, $service_account_authenticator, $enforce_rbac, $authorization_webhook_allowed, $aggregation_proxy_identity, $kubelet_tls, $method, $path_str, $query, $info, $request_field_manager, $admission_metadata, $is_get, $is_list, $is_create, $is_delete, $is_update, $is_watch, $is_certificate_status_subresource, $wants_partial_metadata, $has_body, content_type);
        }
    }};
}
