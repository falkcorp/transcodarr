// file: crates/transcodarr-proto/build.rs
// version: 1.0.0
// guid: 2b7f4c98-0d31-4e6a-95b8-3fa10e274d65
// last-edited: 2026-08-04
//! Compiles the wire contract into `OUT_DIR`.
//!
//! `protoc` is taken from `protoc-bin-vendored` rather than from `PATH`. The
//! alternative makes a build succeed or fail on how a machine happens to be
//! provisioned, and the failure mode is a fresh CI image that has never had it
//! installed — discovered at the worst moment, which is the first time anyone
//! relies on CI.
//!
//! The path is handed to `prost-build` directly rather than exported as
//! `PROTOC`. `std::env::set_var` is on this repository's disallowed list, and
//! rightly: it mutates process-global state that any other thread may be
//! reading, which is why Rust 2024 makes it `unsafe`. A build script happens to
//! be single-threaded today, but passing the value explicitly is both allowed
//! and clearer about where it goes.
//!
//! An explicitly set `PROTOC` still wins, so an operator who needs a specific
//! compiler is not locked out.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    match env::var_os("PROTOC") {
        Some(explicit) => config.protoc_executable(explicit),
        None => config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?),
    };

    let proto = PathBuf::from("proto/transcodarr/v1/agent.proto");
    let include = PathBuf::from("proto");

    // Rebuild when the contract changes, and only then. Without this, cargo
    // re-runs the script on any change in the crate, which is slower and --
    // more importantly -- hides the case where the .proto changed and the
    // generated code did not.
    println!("cargo:rerun-if-changed={}", proto.display());
    println!("cargo:rerun-if-env-changed=PROTOC");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        // The generated client would otherwise carry an inherent
        // `connect(dst)` constructor for dialling a transport, which collides
        // with the client method generated for `rpc Connect` -- same name, same
        // impl block, E0592.
        //
        // Renaming the RPC would resolve it and is the wrong fix: the schema is
        // a reviewed agreement between both ends, and the architecture document
        // names `Connect`. Dropping the convenience constructor costs one line
        // at the call site (`AgentServiceClient::new(channel)`) and leaves the
        // contract alone.
        .build_transport(false)
        .compile_protos_with_config(config, &[proto], &[include])?;

    Ok(())
}
