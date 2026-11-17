//! Generates the Rust types of RustDesk's wire messages
//! (`proto/rustdesk/message.proto`, see its header for where it comes from)
//! for `src/rustdesk`. `protox` compiles the file in pure Rust, so no `protoc`
//! is needed on the build machine.

fn main() {
    let proto = "proto/rustdesk/message.proto";
    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed=build.rs");
    let descriptors = protox::compile([proto], ["proto/rustdesk"]).expect("message.proto compiles");
    prost_build::Config::new()
        // Video frames are megabytes a second: `Bytes` moves them without a copy.
        .bytes(["."])
        .compile_fds(descriptors)
        .expect("Rust types are generated from message.proto");
}
