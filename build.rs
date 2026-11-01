//! Build script. Compiles `proto/gtfs-realtime.proto` with a vendored `protoc`
//! so `cargo install` does not need a system protobuf compiler.

/// Compile the vendored GTFS-Realtime schema into `$OUT_DIR`.
///
/// # Panics
///
/// Panics when the vendored `protoc` binary cannot be found or when `protoc`
/// rejects `proto/gtfs-realtime.proto`.
fn main() {
    println!("cargo:rerun-if-changed=proto/gtfs-realtime.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    prost_build::Config::new()
        .protoc_executable(protoc)
        .compile_protos(&["proto/gtfs-realtime.proto"], &["proto/"])
        .expect("compile gtfs-realtime.proto");
}
