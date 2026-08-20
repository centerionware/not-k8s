//! The generated etcd v3 client types, client-only (`build_server(false)`
//! in `build.rs` — this crate is a client of nodestore, never a server of
//! this API). Mirrors `crates/nodestore/src/pb.rs`'s module structure
//! exactly: the three modules are siblings because prost generates
//! cross-package references as `super::<package>::Type` —
//! `etcdserverpb` refers to `super::mvccpb::KeyValue`, so flattening these
//! into one module or nesting them differently breaks the generated code
//! rather than the other way around.

#![allow(clippy::all)]

pub mod mvccpb {
    tonic::include_proto!("mvccpb");
}

pub mod authpb {
    tonic::include_proto!("authpb");
}

pub mod etcdserverpb {
    tonic::include_proto!("etcdserverpb");
}
