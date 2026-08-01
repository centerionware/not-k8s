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

            println!("cargo:rerun-if-changed=proto/csi.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(&["proto/csi.proto"], &["proto"])
                .expect("failed to compile CSI proto");

            println!("cargo:rerun-if-changed=proto/pluginregistration.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(&["proto/pluginregistration.proto"], &["proto"])
                .expect("failed to compile plugin registration proto");

            println!("cargo:rerun-if-changed=proto/deviceplugin.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(&["proto/deviceplugin.proto"], &["proto"])
                .expect("failed to compile device plugin proto");

            println!("cargo:rerun-if-changed=proto/health.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(&["proto/health.proto"], &["proto"])
                .expect("failed to compile grpc.health.v1 proto");

            println!("cargo:rerun-if-changed=proto/draplugin.proto");
            tonic_prost_build::configure()
                .build_server(false)
                .build_client(true)
                .compile_protos(&["proto/draplugin.proto"], &["proto"])
                .expect("failed to compile DRA plugin proto");

            // PodResources API (round 74): unlike every proto above, nodelet
            // is the SERVER here, not the client — external tooling (device
            // monitoring exporters) dials in to ask what's allocated where.
            println!("cargo:rerun-if-changed=proto/podresources.proto");
            tonic_prost_build::configure()
                .build_server(true)
                .build_client(false)
                .compile_protos(&["proto/podresources.proto"], &["proto"])
                .expect("failed to compile PodResources API proto");
        }
    }
}
