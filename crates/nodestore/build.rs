// Compiles the vendored etcd v3 protos (proto/vendor.sh fetched and stripped
// them; see its header for what was removed and why the wire format is
// unaffected).
//
// Server only. nodestore *is* the etcd endpoint — the client side of this API
// is kube-apiserver, which is not our code. Generating an unused client would
// only add compile time to a binary meant for an edge device.
fn main() {
    for proto in ["kv.proto", "auth.proto", "rpc.proto"] {
        println!("cargo:rerun-if-changed=proto/{proto}");
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/rpc.proto"], &["proto"])
        .expect("failed to compile the etcd v3 protos (is protoc on PATH?)");
}
