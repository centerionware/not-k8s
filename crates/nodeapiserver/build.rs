//! Group A codegen (docs/APISERVER.md): parses the two vendored upstream
//! artifact sets into flat lookup tables everything downstream reads —
//! nothing in this crate hand-maintains a per-type list of protobuf field
//! numbers or patch/SSA metadata.
//!
//! Inputs (checked in, refreshed by `vendor/refresh.sh`):
//!   vendor/protos/**/generated.proto   -> src/codegen/proto_fields.rs
//!   vendor/openapi-spec/v3/*.json      -> src/codegen/openapi_meta.rs
//!
//! Both parsers panic on malformed input rather than emitting a partial
//! table — a build-time failure here is far cheaper to diagnose than a
//! runtime KeyError deep in the codec or patch logic.
//!
//! Also compiles the etcd v3 gRPC client (Group C) from `proto/rpc.proto`
//! — a synced copy of `nodestore`'s own already-vendored, already-stripped
//! protos (`proto/sync-from-nodestore.sh`), client-only
//! (`build_server(false)`): this crate is a client of nodestore, never a
//! server of this API. See `crates/nodestore/build.rs` for the precedent
//! this mirrors.

#[path = "build/proto_parse.rs"]
mod proto_parse;

#[path = "build/openapi_parse.rs"]
mod openapi_parse;

#[path = "build/discovery_parse.rs"]
mod discovery_parse;

#[path = "build/openapi_serve.rs"]
mod openapi_serve;

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let vendor_dir = Path::new(&manifest_dir).join("vendor");
    let proto_dir = vendor_dir.join("protos");
    let openapi_dir = vendor_dir.join("openapi-spec/v3");

    println!("cargo:rerun-if-changed={}", proto_dir.display());
    println!("cargo:rerun-if-changed={}", openapi_dir.display());

    assert!(
        proto_dir.is_dir(),
        "{} not found — run vendor/refresh.sh to vendor the upstream .proto files first",
        proto_dir.display()
    );
    assert!(
        openapi_dir.is_dir(),
        "{} not found — run vendor/refresh.sh to vendor the upstream OpenAPI v3 specs first",
        openapi_dir.display()
    );

    let (proto_fields, proto_messages) = proto_parse::parse_all(&proto_dir);
    assert!(!proto_fields.is_empty(), "parsed zero protobuf fields out of {}", proto_dir.display());
    let proto_out = proto_parse::render(&proto_fields, &proto_messages);

    let (field_meta, gvks, required, types) = openapi_parse::parse_all(&openapi_dir);
    assert!(!gvks.is_empty(), "parsed zero discovery GVKs out of {}", openapi_dir.display());
    assert!(!required.is_empty(), "parsed zero required-field entries out of {}", openapi_dir.display());
    assert!(!types.is_empty(), "parsed zero type-info entries out of {}", openapi_dir.display());
    let openapi_out = openapi_parse::render(&field_meta, &gvks, &required, &types);

    let resources = discovery_parse::parse_all(&openapi_dir);
    assert!(!resources.is_empty(), "parsed zero per-version API resources out of {}", openapi_dir.display());
    let discovery_out = discovery_parse::render(&resources);

    let served_docs = openapi_serve::parse_all(&openapi_dir);
    assert!(!served_docs.is_empty(), "found zero servable OpenAPI v3 docs in {}", openapi_dir.display());
    let served_docs_out = openapi_serve::render(&served_docs);

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let out_dir = Path::new(&out_dir);
    std::fs::write(out_dir.join("proto_fields.rs"), proto_out).expect("writing proto_fields.rs");
    std::fs::write(out_dir.join("openapi_meta.rs"), openapi_out).expect("writing openapi_meta.rs");
    std::fs::write(out_dir.join("api_resources.rs"), discovery_out).expect("writing api_resources.rs");
    std::fs::write(out_dir.join("openapi_v3_docs.rs"), served_docs_out).expect("writing openapi_v3_docs.rs");

    println!(
        "cargo:warning=nodeapiserver codegen: {} protobuf fields across {} messages, {} field-meta entries, {} discovery GVKs, {} required-field entries, {} type-info entries, {} per-version API resources, {} servable OpenAPI v3 docs",
        proto_fields.len(),
        proto_messages.len(),
        field_meta.len(),
        gvks.len(),
        required.len(),
        types.len(),
        resources.len(),
        served_docs.len()
    );

    println!("cargo:rerun-if-changed=proto/rpc.proto");
    println!("cargo:rerun-if-changed=proto/kv.proto");
    println!("cargo:rerun-if-changed=proto/auth.proto");
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/rpc.proto"], &["proto"])
        .expect("failed to compile the etcd v3 client protos (is protoc on PATH?)");

    // `/version` (server::version): a handful of build-time facts real
    // upstream embeds via `-ldflags`, this crate's own equivalent since
    // Cargo has no linker-flag string injection. Every command here
    // degrades to a named "unknown" on failure rather than aborting the
    // build — none of this is essential to a working binary, only to a
    // fully-populated /version response, so a build host missing `git`
    // (a tarball checkout, not a clone) must still build.
    // Best-effort staleness trigger only — a build host with no `.git` at
    // all (a release tarball) just never reruns this step, which is fine
    // since it has nothing to react to either.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        std::process::Command::new(cmd).args(args).current_dir(&manifest_dir).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let git_commit = run("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let git_tree_state = match run("git", &["status", "--porcelain"]) {
        Some(s) if s.is_empty() => "clean".to_string(),
        Some(_) => "dirty".to_string(),
        None => "unknown".to_string(),
    };
    let build_date = run("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".to_string());
    let rustc_version = run("rustc", &["--version"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NODEAPISERVER_GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=NODEAPISERVER_GIT_TREE_STATE={git_tree_state}");
    println!("cargo:rustc-env=NODEAPISERVER_BUILD_DATE={build_date}");
    println!("cargo:rustc-env=NODEAPISERVER_RUSTC_VERSION={rustc_version}");
}
