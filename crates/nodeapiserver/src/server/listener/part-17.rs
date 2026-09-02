
fn is_authorization_review(info: &path::RequestInfo) -> bool {
    (info.api_group == "authorization.k8s.io"
        && matches!(
            info.resource.as_str(),
            "subjectaccessreviews"
                | "selfsubjectaccessreviews"
                | "localsubjectaccessreviews"
                | "selfsubjectrulesreviews"
        ))
        || (info.api_group == "authentication.k8s.io" && info.resource == "selfsubjectreviews")
}

fn should_run_local_authorization(
    info: &path::RequestInfo,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
) -> bool {
    enforce_rbac
        && !authorization_webhook_allowed
        && info.is_resource_request
        && !is_authorization_review(info)
}
