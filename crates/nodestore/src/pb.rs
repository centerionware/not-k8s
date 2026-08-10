//! The generated etcd v3 types.
//!
//! The three modules are siblings because prost generates cross-package
//! references as `super::<package>::Type` — `etcdserverpb` refers to
//! `super::mvccpb::KeyValue`, so flattening these into one module or nesting
//! them differently breaks the generated code rather than the other way
//! around.

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
