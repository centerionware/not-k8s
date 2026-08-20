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

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let out_dir = Path::new(&out_dir);
    std::fs::write(out_dir.join("proto_fields.rs"), proto_out).expect("writing proto_fields.rs");
    std::fs::write(out_dir.join("openapi_meta.rs"), openapi_out).expect("writing openapi_meta.rs");

    println!(
        "cargo:warning=nodeapiserver codegen: {} protobuf fields across {} messages, {} field-meta entries, {} discovery GVKs, {} required-field entries, {} type-info entries",
        proto_fields.len(),
        proto_messages.len(),
        field_meta.len(),
        gvks.len(),
        required.len(),
        types.len()
    );

    println!("cargo:rerun-if-changed=proto/rpc.proto");
    println!("cargo:rerun-if-changed=proto/kv.proto");
    println!("cargo:rerun-if-changed=proto/auth.proto");
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/rpc.proto"], &["proto"])
        .expect("failed to compile the etcd v3 client protos (is protoc on PATH?)");
}
