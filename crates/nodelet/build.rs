// Only compiles the CRI protobufs when the `cri` feature is enabled.
// The default build needs no protoc and no network — just the mock runtime.
fn main() {
    if std::env::var("CARGO_FEATURE_CRI").is_ok() {
        #[cfg(feature = "cri")]
        {
            println!("cargo:rerun-if-changed=proto/cri.proto");
            println!("cargo:rerun-if-changed=proto/containerd_events.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(
                    &["proto/cri.proto", "proto/containerd_events.proto"],
                    &["proto"],
                )
                .expect("failed to compile CRI/events protos");
        }
    }
}
