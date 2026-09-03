//! Group E: real, generic REST verbs wired against actual nodestore data.
//! Closes the gap `docs/APISERVER.md`
//! has named repeatedly ("actually wiring discovery/defaulting/validation
//! into a live request path"): everything this function needs (path
//! grammar, storage key layout, the protobuf wire codec, the discovery
//! table telling it which Kind a resource serves) already existed —
//! generic over every resource this build knows about, not hand-written
//! per type, same "generic over vendored data" posture every other Group
//! B/C/E slice has taken.
//!
//! # Scope, named honestly
//!
//! `GET` (single object, `GET /api/v1/namespaces/{ns}/pods/{name}`-shaped),
//! `LIST` (`GET /api/v1/namespaces/{ns}/pods`-shaped, no name), `CREATE`
//! (`POST /api/v1/namespaces/{ns}/pods`), single-object `DELETE`
//! (`DELETE /api/v1/namespaces/{ns}/pods/{name}`), `UPDATE`
//! (`PUT /api/v1/namespaces/{ns}/pods/{name}`), and now `PATCH`
//! (`PATCH /api/v1/namespaces/{ns}/pods/{name}`, real optimistic
//! concurrency against the exact revision `patch_prepare` itself reads,
//! no client-submitted `resourceVersion` required — see [`patch_prepare`]/
//! [`patch_persist`]'s own doc comments for the three real patch kinds,
//! reusing Group G's already-landed `patch::json_patch`/`merge_patch`/
//! `strategic_merge`, and for why the function is split in two —
//! `server::listener` runs Group J admission against the real candidate
//! object in between), and now [`delete_collection`]
//! (`DELETE /api/v1/namespaces/{ns}/pods`, no name — lists via the same
//! selector filtering [`list`] already has, then deletes each match,
//! returning the pre-deletion `List` real upstream's own
//! `Store.DeleteCollection` returns) — `watch` remains the only verb this
//! build knows about that isn't a generic REST dispatch (a real
//! streaming response instead, `server::listener`'s own doc comment).
//! `DELETECOLLECTION` is also passed through Group J admission one matched
//! object at a time by `server::listener`, matching the generic upstream
//! store's validation callback. `get` and `list` can both
//! consult a `cacher::store::SharedCache` if the caller passes one — see
//! each function's own doc comment for its exact contract (`get`: a hit
//! skips nodestore, a miss always falls through to a real `Range` rather
//! than trusting the cache to say "not found"; `list`: only once the
//! cache's own `has_synced()` is true, since an empty `list()` is a
//! valid answer on its own, not a fallthrough signal the way a `get`
//! miss is). `server::listener` actually does this for every built-in
//! resource in the generated discovery table; dynamically defined CRD
//! resources are still registered lazily after discovery. `create`/`update`/
//! `delete` still read/write
//! straight to `storage::client::StorageClient` directly, bypassing the
//! cache entirely — a real, valid strategy (upstream's own quorum-read /
//! watch-cache-disabled path takes exactly this shape), not a shortcut.
//! No authentication is consulted *inside*
//! this module either way — `server::listener` is what applies Group
//! H/I's identity/RBAC (opt-in, see that module's own doc comment)
//! before ever calling in here; Group J admission (eight unconditional
//! plugins as of this revision — see `admission`'s own doc comment) is
//! applied in `server::listener`, also before dispatching in here.
//! The generic `<resource>/status` subresource is real now
//! (`update_status`/`patch_status`), as is the core Pod
//! `ephemeralcontainers` subresource (`get_ephemeral_containers`,
//! `update_ephemeral_containers`, and `patch_ephemeral_containers`);
//! the core Pod `resize` subresource is also real (`GET`/`PUT`/`PATCH`),
//! restricting writes to container resources and resize policies;
//! built-in workload `scale` subresources are translated to the parent's
//! `spec.replicas`; every other subresource (`pods/log`, ...) still isn't,
//! except for the scheduler's `pods/binding` subresource — the discovery
//! table this module reads doesn't carry their handler semantics either (a
//! named, separate skip in `build/discovery_parse.rs`). `list` filters by
//! label/field selector
//! for real (`cacher::selector::object_matches`, wired against every
//! item's own decoded JSON — Group D's own generic adapter, unchanged
//! here) and paginates for real too (`limit`/`continue_token`, its own
//! opaque resume-key encoding — see `list`'s own doc comment). `get` and
//! `list` also honor a positive `resourceVersion` by reading a consistent
//! nodestore MVCC snapshot; pinned requests bypass the live watch cache.
//!
//! `create` runs Group F's already-landed `scheme::validation`
//! (`validate_required`/`validate_types`, on the client's raw submitted
//! body — required-ness is about what the *user* sent, not what survives
//! defaulting, same order those functions' own doc comments already
//! specify) then `scheme::defaulting::apply_defaults`, sets
//! `metadata.creationTimestamp`/`uid` for real, and writes with a real
//! create-only-if-absent `Txn` (`Compare(ModRevision(key), Equal, 0)` —
//! confirmed directly against `nodestore`'s own server-side comment
//! naming this the idiom for "create only if absent," not assumed).
//! `name_format_violations` also wires `scheme::name_format`'s
//! validators in for the core-group resources this crate has actually
//! verified a real per-type rule for: `namespaces` -> `is_dns1123_label`
//! (`ValidateNamespaceName`), `services` -> `is_dns1035_label`
//! (`ValidateServiceName`, ignoring the alpha
//! `RelaxedServiceNameValidation` feature gate this crate has no
//! machinery for), and twenty-six resources sharing
//! `is_dns1123_subdomain` (core group: `serviceaccounts`, `pods`,
//! `replicationcontrollers`, `nodes`, `limitranges`, `resourcequotas`,
//! `secrets`, `endpoints`, `persistentvolumes`, `configmaps`; non-core,
//! each individually group-verified against the vendored spec:
//! `scheduling.k8s.io/priorityclasses`,
//! `resource.k8s.io/resourceclaims`,
//! `resource.k8s.io/resourceclaimtemplates`,
//! `storage.k8s.io/storageclasses`, `apps/controllerrevisions`,
//! `apps/daemonsets`, `apps/deployments`, `apps/replicasets`,
//! `networking.k8s.io/ingresses`, `networking.k8s.io/ingressclasses`,
//! `networking.k8s.io/servicecidrs`, `discovery.k8s.io/endpointslices`,
//! `flowcontrol.apiserver.k8s.io/flowschemas`,
//! `flowcontrol.apiserver.k8s.io/prioritylevelconfigurations`,
//! `node.k8s.io/runtimeclasses`, `coordination.k8s.io/leases`) — every
//! other resource is
//! deliberately left unchecked rather than guessed at; see that
//! function's own doc comment for how to extend it one verified entry at
//! a time. `update` runs the exact same two checks. `create` and `update`
//! also expose the listener's `dryRun=All` path, which returns the fully
//! prepared object without persisting it. Server-Side Apply bookkeeping is
//! handled by the separate apply path.
//!
//! `delete` reads the object, checks optional `resourceVersion`/`uid`
//! preconditions (`metav1.DeleteOptions.Preconditions`), and uses an MVCC
//! compare with `DeleteRange` so a concurrent update cannot invalidate the
//! check. It returns the deleted object, matching real upstream's own
//! synchronous delete response. Finalizers are honored: a delete marks an
//! object with `deletionTimestamp` and the object is removed when its last
//! finalizer is cleared. `propagationPolicy` remains out of scope.
//!
//! `update` is real optimistic concurrency, not a blind overwrite: reads
//! the current object first, requires the submitted body's own
//! `metadata.resourceVersion` to match what's actually stored (a real
//! `Conflict`, not a silent clobber, on a mismatch — and a real
//! `MissingResourceVersion` outcome if the client omitted it, matching
//! real upstream's own requirement for `PUT`), then writes with a `Txn`
//! compared against that exact revision, so a concurrent write between
//! the read and this write also loses the race rather than being
//! silently overwritten. `metadata.creationTimestamp`/`uid` are always
//! preserved from the existing object regardless of what the client
//! submitted — both are immutable after creation, matching real
//! upstream. No create-on-update (a request targeting a name that
//! doesn't exist is rejected, not created — real upstream's
//! `AllowCreateOnUpdate` opt-in a handful of types use isn't modeled).

use crate::apiextensions;
use crate::cacher::selector::{self, ParseError};
use crate::codec::protobuf;
use crate::codegen;
use crate::scheme::{defaulting, validation};
use crate::storage::client::{Error as StorageError, StorageClient, prefix_range_end};
use crate::storage::encryption::Transformer;
use crate::storage::keys;
use crate::storage::pb::etcdserverpb as pb;
use crate::storage::pb::etcdserverpb::RangeRequest;
use crate::storage::pb::mvccpb;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("nodestore request failed: {0}")]
    Storage(#[from] StorageError),
    #[error("decoding the stored object failed: {0}")]
    Decode(#[from] protobuf::Error),
    #[error("invalid selector: {0}")]
    Selector(#[from] ParseError),
    #[error("encryption transform failed: {0}")]
    Encryption(#[from] crate::storage::encryption::Error),
    #[error("invalid protobuf request: {0}")]
    InvalidProtobufRequest(String),
    #[error("the requested resource is not served")]
    UnknownResource,
}

#[derive(Debug, PartialEq)]
pub enum GetOutcome {
    /// The decoded object, ready to serialize.
    Found(Value),
    /// This build has no such `(group, version, resource)` at all — same
    /// "real 404, not a silent fallthrough" reasoning
    /// `server::discovery`'s own `NotFound` case already established.
    UnknownResource,
    /// The resource is known, but no object exists at that key.
    ObjectNotFound,
}

/// The `Kind` this build serves at `(group, version, resource)`, or
/// `None` if this build doesn't know that resource at all. Pure and
/// unit-tested apart from [`get`]'s own network call.
pub fn resolve_kind(group: &str, version: &str, resource: &str) -> Option<&'static str> {
    codegen::api_resources_by_group_version()
        .get(&(group, version))?
        .iter()
        .find(|r| r.resource == resource)
        .map(|r| r.kind)
}

/// Resolves a parameter kind from a `ValidatingAdmissionPolicy`'s
/// `spec.paramKind`. Parameter kinds carry an API group and Kind but no
/// version or resource plural, so choose the most-preferred served version
/// from the static discovery table, then fall back to an Established CRD.
/// This is intentionally a read-only inverse of the normal resource lookup;
/// callers still use [`get`]` and [`list`]` for the actual parameter object.
pub async fn resolve_resource_for_kind(
    storage: &mut StorageClient,
    group: &str,
    kind: &str,
) -> Result<Option<(String, String, String, bool)>, Error> {
    let mut static_matches = codegen::api_resources::API_RESOURCES
        .iter()
        .filter(|resource| resource.group == group && resource.kind == kind)
        .collect::<Vec<_>>();
    static_matches.sort_by(|left, right| {
        super::version_compare::compare_kube_aware_versions(&right.version, &left.version)
    });
    if let Some(resource) = static_matches.into_iter().next() {
        return Ok(Some((
            resource.group.to_string(),
            resource.version.to_string(),
            resource.resource.to_string(),
            resource.namespaced,
        )));
    }

    let mut dynamic_matches =
        apiextensions::registry::discoverable_resources(list_stored_crds(storage).await?.iter())
            .into_iter()
            .filter(|resource| resource.group == group && resource.kind == kind)
            .collect::<Vec<_>>();
    dynamic_matches.sort_by(|left, right| {
        super::version_compare::compare_kube_aware_versions(&right.version, &left.version)
    });
    Ok(dynamic_matches.into_iter().next().map(|resource| {
        (
            resource.group,
            resource.version,
            resource.resource,
            resource.namespaced,
        )
    }))
}

/// Resolve the served resource's namespacedness for admission matching. The
/// static discovery table handles built-ins without I/O; a CRD lookup uses
/// the same established definitions as ordinary REST resolution.
pub async fn resource_is_namespaced(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<Option<bool>, Error> {
    if let Some(found) = codegen::api_resources_by_group_version()
        .get(&(group, version))
        .and_then(|resources| {
            resources
                .iter()
                .find(|candidate| candidate.resource == resource)
        })
    {
        return Ok(Some(found.namespaced));
    }
    let crds = list_stored_crds(storage).await?;
    Ok(apiextensions::registry::discoverable_resources(crds.iter())
        .into_iter()
        .find(|candidate| {
            candidate.group == group
                && candidate.version == version
                && candidate.resource == resource
        })
        .map(|candidate| candidate.namespaced))
}

include!("rest/resolve.rs");
include!("rest/read.rs");
include!("rest/create.rs");
include!("rest/types.rs");
include!("rest/scale.rs");
include!("rest/subresources.rs");
include!("rest/update.rs");
include!("rest/patch.rs");
include!("rest/pod_resize.rs");
include!("rest/apply.rs");
include!("rest/apply_helpers.rs");
include!("rest/delete.rs");
include!("rest_tests.rs");
