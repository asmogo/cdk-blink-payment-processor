fn main() {
    println!("cargo:rerun-if-changed=src/payment_processor.proto");
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile(&["src/payment_processor.proto"], &["src"])
        .unwrap();
}
