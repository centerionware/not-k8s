// Compiles three sets of protos:
//
//   rpc.proto     — the etcd v3 API (vendored; see proto/vendor.sh). Server
//                   AND client: the server is nodestore's whole purpose, and
//                   the client is how a follower forwards a write to the
//                   leader. Forwarding the caller's own request verbatim, with
//                   the same generated types, is far less machinery than
//                   inventing a second internal encoding for every operation
//                   — and it cannot drift from the API it is proxying.
//   command.proto — the raft log entry and snapshot format. Ours.
//   peer.proto    — node-to-node raft transport. Ours.
fn main() {
    for proto in ["kv.proto", "auth.proto", "rpc.proto", "command.proto", "peer.proto"] {
        println!("cargo:rerun-if-changed=proto/{proto}");
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/rpc.proto"], &["proto"])
        .expect("failed to compile the etcd v3 protos (is protoc on PATH?)");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/command.proto", "proto/peer.proto"], &["proto"])
        .expect("failed to compile the log/transport protos");
}
