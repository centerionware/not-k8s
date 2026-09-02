
/// The three real patch media types this build understands, and which
/// `patch::*` module applies each. The request handler separately applies
/// Kubernetes' default strategy when a request has no `Content-Type`:
/// strategic merge for built-in resources and merge patch for CRDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    Json,
    Merge,
    StrategicMerge,
}

/// Real upstream's own three patch `Content-Type` media types
/// (`k8s.io/apimachinery/pkg/types`): `application/json-patch+json`
/// (RFC 6902), `application/merge-patch+json` (RFC 7386),
/// `application/strategic-merge-patch+json` (k8s-specific). Server-Side
/// Apply's own `application/apply-patch+yaml` is routed separately by the
/// listener because it has different query parameters and bookkeeping than
/// the three ordinary patch kinds below.
pub fn patch_kind_for_content_type(content_type: &str) -> Option<PatchKind> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => Some(PatchKind::Json),
        "application/merge-patch+json" => Some(PatchKind::Merge),
        "application/strategic-merge-patch+json" => Some(PatchKind::StrategicMerge),
        _ => None,
    }
}

/// Kubernetes' default patch strategy when a request omits `Content-Type`.
/// Built-in resources have compiled schemas and therefore use strategic
/// merge; CRD-defined resources use JSON merge patch because they do not
/// have the generated strategic-merge metadata used by built-ins.
pub fn default_patch_kind(is_crd: bool) -> PatchKind {
    if is_crd { PatchKind::Merge } else { PatchKind::StrategicMerge }
}

/// Resolves the resource and returns the default patch strategy for a
/// request with no `Content-Type`. `None` means the URL names no resource
/// this server knows about, so the listener can preserve its normal 404
/// response rather than reporting a media-type error.
pub async fn default_patch_kind_for_request(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<PatchKind>, Error> {
    Ok(resolve_resource(storage, group, version, resource).await?.map(|resolved| default_patch_kind(resolved.schema.is_none())))
}
