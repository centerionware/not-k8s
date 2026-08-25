fn main() {
    println!("cargo:rerun-if-changed=../nodelet/proto/pluginregistration.proto");
    println!("cargo:rerun-if-changed=../nodelet/proto/deviceplugin.proto");
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
}
