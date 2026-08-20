//! nodeapiserver — kube-apiserver's job: REST + watch over every built-in
//! and CRD-defined resource, backed by `nodestore`. The last k3s component;
//! see `docs/APISERVER.md` for the design, group-by-group status, and the
//! `nodeapiserver` integration-branch merge model this crate is developed
//! under.
//!
//! Everything below the process boundary lives here (not `main.rs`), so the
//! combined `notk8s` binary can run exactly this code path without a second
//! copy of the dependency tree — same convention every other component in
//! this workspace follows.
//!
//! # Module map
//!
//! Mirrors `docs/APISERVER.md`'s delivery groups so the two can be read
//! side by side. Groups not yet landed have an empty or near-empty module
//! present as a placeholder — see each module's own doc comment for its
//! real status; this file does not duplicate that bookkeeping.
//!
//!   `codegen`      — Group A: build-time-generated protobuf field table and
//!                    OpenAPI SMP/SSA/discovery metadata (`build.rs`).
//!   `codec`        — Group B: JSON/YAML/protobuf wire formats.
//!   `storage`      — Group C: the nodestore (etcd v3) client.
//!   `cacher`       — Group D: the watch cache.
//!   `server`       — Group E: listener, handler chain, path grammar.
//!   `scheme`       — Group F: GVK registry, conversion, defaulting.
//!   `patch`        — Group G: JSON/merge/strategic patch, SSA.
//!   `authn`        — Group H: authentication.
//!   `authz`        — Group I: authorization.
//!   `admission`    — Group J: admission plugins and webhooks.
//!   `cel_ext`      — Groups J/K: cost budget, k8s extension libraries,
//!                    type-checking. Named `cel_ext`, not `cel` (the name
//!                    `docs/APISERVER_PLAN.md` uses) — this crate also
//!                    depends on the external `cel` crate, and a top-level
//!                    `mod cel` sitting alongside it in the same crate root
//!                    would shadow it for every `use cel::...` elsewhere in
//!                    this crate.
//!   `apiextensions`— Group K: CRDs.
//!   `aggregator`   — Group L: APIService proxying.
//!   `flowcontrol`  — Group M: APF.
//!   `audit`        — Group M: audit pipeline.
//!   `proxy`        — Group N: exec/attach/port-forward/log splice to nodelet.
//!   `bootstrap`    — Group O: cluster PKI, RBAC policy, kubernetes Service, addons.

pub mod codegen;
pub mod config;

pub mod scheme;
pub mod codec;
pub mod storage;
pub mod cacher;
pub mod server;
pub mod registry;
pub mod patch;
pub mod authn;
pub mod authz;
pub mod admission;
pub mod cel_ext;
pub mod apiextensions;
pub mod aggregator;
pub mod flowcontrol;
pub mod audit;
pub mod proxy;
pub mod bootstrap;

use anyhow::Result;

/// Entry point shared by the split `nodeapiserver` binary and the combined
/// `notk8s` applet dispatch (once Group O adds the `components.sh` row —
/// docs/APISERVER_PLAN.md finding 11 — this is what it will call).
///
/// Runs the Group E listener forever. **Its handler is still a bring-up
/// stub** (`server::listener`'s own doc comment) — this proves the
/// listener, TLS, and path grammar work end to end, not that the apiserver
/// is feature-complete. Not wired into `deploy/lib/components.sh` yet for
/// exactly that reason (Group O's job, once there's a real handler chain
/// behind it).
pub async fn run() -> Result<()> {
    let cfg = config::Config::from_env()?;
    tracing::info!(
        proto_messages = codegen::proto_fields::PROTO_MESSAGES.len(),
        proto_fields = codegen::proto_fields::PROTO_FIELDS.len(),
        discovery_gvks = codegen::openapi_meta::DISCOVERY_GVKS.len(),
        field_meta_entries = codegen::openapi_meta::FIELD_META.len(),
        bind_addr = %cfg.bind_addr,
        "nodeapiserver starting"
    );
    server::listener::run(cfg).await;
    Ok(())
}
