//! Generated protobuf wire types for the call body — the Rust analogue of the
//! TS `call.proto.gen.cjs` static module.
//!
//! The schema lives in `proto/call.proto`; `build.rs` codegen's it (via
//! `protox` + `prost-build`) into `$OUT_DIR/sipjsserver.call.rs`, which is
//! `include!`d below under the proto package path `sipjsserver::call`. Every
//! message implements [`prost::Message`], so `.encode_to_vec()` / `decode(..)`
//! give the wire form directly.
//!
//! This module is the **schema + codegen toolchain** only. The protobuf
//! `CallBodyCodec` impl — the `Call` (model) ↔ `sipjsserver::call::Call` (wire)
//! mapping, with the `*IsNull` / `*Present` side-channels and the JSON-string
//! carries (`featuresJson`, `extJson`, `pendingInviteTxnJson`, …) the schema's
//! comments describe — is a separate item that stacks on this one. Until then
//! these types stand alone, exercised by the wire-level round-trip tests in
//! `tests/proto_codegen.rs`.
//!
//! ## Naming
//!
//! prost lowercases proto3 `camelCase` fields to `snake_case` Rust idents
//! (`outboundCSeq` → `outbound_c_seq`, `aLegInvite` → `a_leg_invite`) and
//! r#-escapes keyword fields (`type` → `r#type`). `optional` proto fields become
//! `Option<T>`; `repeated` become `Vec<T>`; `bytes` become `Vec<u8>`. Message
//! fields are `Option<Submessage>` because proto3 message presence is optional.

/// The `sipjsserver.call` protobuf package. Mirrors the TS
/// `sipjsserver.call` namespace from `call.proto.gen.cjs`.
pub mod sipjsserver {
    /// The `call` sub-namespace: every message declared in `proto/call.proto`.
    pub mod call {
        include!(concat!(env!("OUT_DIR"), "/sipjsserver.call.rs"));
    }
}

/// Re-export of the generated package leaf so downstream code can write
/// `call::proto::wire::Call` for the wire `Call` without the full package path.
pub use sipjsserver::call as wire;
