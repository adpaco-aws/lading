fn main() {
    println!("cargo:rerun-if-changed=proto/",);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let payloads_proto = format!("{manifest_dir}/../../");

    let includes = ["proto/", &payloads_proto];
    prost_build::Config::new()
        .out_dir("src/proto")
        .extern_path(".libs", "")
        .compile_protos(&["proto/capture/v1/capture.proto"], &includes)
        .unwrap();
}
