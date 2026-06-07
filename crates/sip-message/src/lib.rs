//! sip-message — the pure SIP message layer (Rust port of sipjsserver's
//! `src/sip` message core). Slice 1 of the migration: parse / serialize /
//! strict-validate. No async, no I/O, no clock — a pure leaf crate.
//!
//! See MIGRATION_STATUS.md for what is ported vs. pending, and
//! docs/MIGRATION_STRATEGY.md for the decisions behind this layout.

pub mod error;
pub mod method;
pub mod types;
pub mod parser;

// Slice-1 modules, scaffolded; ported incrementally (see MIGRATION_STATUS.md).
pub mod serializer;
pub mod sdp;
pub mod generators;
pub mod message_helpers;
pub mod sipfrag;

pub use error::SipParseError;
pub use method::Method;
pub use serializer::{message_summary, serialize, sip_summary};
pub use sdp::{
    build_answer_from_offer, build_held_sdp_from_profile, extract_codec_profile, validate_sdp_body,
    BuildAnswerOptions, BuildHeldSdpOptions, CodecProfile, SdpBuildResult, SdpValidationError,
};
pub use parser::{SipParser, SipParserLimits};
pub use parser::custom::{hydrate_request, hydrate_response, CustomParser};
pub use types::{
    Contact, ContactSet, CSeq, InDialogRequest, InviteRequest, NameAddr, NonEmpty, NotInDialog,
    OptionalHeaders, Params, ParamValue, Rack, ReferTo, Replaces, RequestUri, SipHeader,
    SipMessage, SipRequest, SipResponse, SipResponseTagged, TypedHeader, Uri, Via,
};
