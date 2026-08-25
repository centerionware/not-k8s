fn main() {
    println!("cargo:rerun-if-changed=../nodelet/proto/pluginregistration.proto");
    println!("cargo:rerun-if-changed=../nodelet/proto/deviceplugin.proto");
    println!("cargo:rerun-if-changed=../nodestore/proto/rpc.proto");
    println!("cargo:rerun-if-changed=../nodestore/proto/kv.proto");
    println!("cargo:rerun-if-changed=../nodestore/proto/auth.proto");
    println!("cargo:rerun-if-changed=../nodestore/proto/peer.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &[
                "../nodelet/proto/pluginregistration.proto",
                "../nodelet/proto/deviceplugin.proto",
            ],
            &["../nodelet/proto"],
        )
        .expect("failed to compile nodelet plugin protos");
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &[
                "../nodestore/proto/rpc.proto",
                "../nodestore/proto/peer.proto",
            ],
            &["../nodestore/proto"],
        )
        .expect("failed to compile nodestore client protos");
}
