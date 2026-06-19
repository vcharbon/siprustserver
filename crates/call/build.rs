//! Codegen the protobuf call-body wire types from `proto/call.proto`.
//!
//! This is the Rust analogue of the TS `pbjs --target static-module` step that
//! produces `src/call/codec/call.proto.gen.cjs` in the source tree. Here the
//! generated module is an OUT_DIR build artifact (never committed), `include!`d
//! by `src/proto.rs`.
//!
//! ## Why `protox` instead of `protoc`
//!
//! `prost-build` 0.14 does **not** bundle `protoc`; its `compile_protos` shells
//! out to a `protoc` on PATH. There is no system `protoc` in this toolchain, so
//! we compile the schema to a `FileDescriptorSet` with [`protox`] — a pure-Rust
//! protobuf compiler — and feed that to
//! [`prost_build::Config::compile_fds`]. The build is then reproducible with no
//! external binary.
//!
//! ## Output
//!
//! Package `sipjsserver.call` → prost writes `$OUT_DIR/sipjsserver.call.rs`. It
//! is wired into the crate as the `sipjsserver::call` module by `src/proto.rs`.
//!
//! ## Schema stability (ADR-0011)
//!
//! A codec swap is a fresh-cluster event (drain + FLUSHDB + redeploy), so wire
//! compatibility across releases is intentionally not maintained. *Within* a
//! release the field-id rules are: never reuse a deleted id; append-only per
//! message; never reorder. Those rules live in the `.proto` comments — this file
//! just compiles whatever the schema currently declares.

use std::path::Path;

fn main() {
    let proto = "proto/call.proto";
    let proto_dir = "proto";

    // Regenerate when the schema (or its directory, e.g. an added import) moves.
    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed={proto_dir}");

    assert!(
        Path::new(proto).exists(),
        "missing {proto}; the protobuf wire schema must ship with the crate"
    );

    // Pure-Rust .proto → FileDescriptorSet (no system `protoc`). `protox`
    // surfaces a `miette`-rendered diagnostic on a malformed schema.
    let fds = protox::compile([proto], [proto_dir])
        .unwrap_or_else(|e| panic!("protox failed to compile {proto}: {e}"));

    // FileDescriptorSet → Rust types in OUT_DIR. `compile_fds` does the same
    // codegen as `compile_protos` but takes the already-parsed descriptors, so
    // it never invokes an external `protoc`.
    prost_build::Config::new()
        .compile_fds(fds)
        .unwrap_or_else(|e| panic!("prost-build failed to generate types for {proto}: {e}"));
}
