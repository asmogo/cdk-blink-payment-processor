fn main() {
    println!("cargo:rerun-if-changed=src/payment_processor.proto");
    // Generate Rust types and a file descriptor set for reflection (write it to OUT_DIR)
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR env var not set by Cargo");
    let descriptor_path =
        std::path::PathBuf::from(&out_dir).join("cdk_payment_processor_descriptor.bin");

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&["src/payment_processor.proto"], &["src"]) // renamed in tonic-build 0.12
        .unwrap();
}
