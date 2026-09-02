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
//! before ever calling in here; Group J admission (five unconditional
//! plugins as of this revision — see `admission`'s own doc comment) is
//! applied in `server::listener`, also before dispatching in here.
//! The generic `<resource>/status` subresource is real now
//! (`update_status`/`patch_status`), as is the core Pod
//! `ephemeralcontainers` subresource (`get_ephemeral_containers`,
//! `update_ephemeral_containers`, and `patch_ephemeral_containers`);
//! every other subresource (`pods/log`, ...) still isn't, except for the
//! scheduler's `pods/binding` subresource — the discovery table this
//! module reads doesn't carry them either (a named, separate skip in
//! `build/discovery_parse.rs`). `list` filters by label/field selector
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
use crate::storage::encryption::Transformer;
use crate::storage::client::{prefix_range_end, Error as StorageError, StorageClient};
use crate::storage::keys;
use crate::storage::pb::etcdserverpb as pb;
use crate::storage::pb::etcdserverpb::RangeRequest;
use crate::storage::pb::mvccpb;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
include!("rest/part-1.rs");
include!("rest/part-2.rs");
include!("rest/part-3.rs");
include!("rest/part-4.rs");
include!("rest/part-5.rs");
