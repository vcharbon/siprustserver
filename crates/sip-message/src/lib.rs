//! sip-message — the pure SIP message layer: parse / serialize /
//! strict-validate plus read/rewrite helpers. No async, no I/O, no clock —
//! a pure leaf crate.
//!
//! This is the ONLY crate that extracts SIP headers/messages. Lenient
//! raw-datagram scanning: [`sniff`]; strict pre-parse classifiers:
//! [`preparse`]; parsed-header access: the typed read surface on
//! [`SipRequest`] / [`SipResponse`] ([`header`] values, [`HeaderName`]);
//! construction: [`draft`] and [`generators`].

pub mod error;
pub mod method;
pub mod sip_str;
pub mod types;
pub mod header;
pub mod draft;
mod access;
pub mod parser;

pub mod serializer;
pub mod sdp;
pub mod sdp_diff;
pub mod sdp_doc;
pub mod generators;
pub mod emergency;
pub mod hops;
pub mod param_codec;
pub mod preparse;
pub mod sipfrag;
pub mod sniff;
pub mod deviation;
pub mod remote_target;
pub mod multipart;
pub mod projection;
pub mod template;
pub mod template_match;
pub mod trace_sample;

/// The body and image type the public surface hands out and takes back.
pub use bytes::Bytes;
pub use error::SipParseError;
/// Header identity and the one ownership table (ADR-0025).
pub use header::{canonical_header_items, header_forms_equivalent, HeaderClass, HeaderName};
/// RFC 3261 §7.3.3 compact-form expansion, probed one name at a time.
pub use parser::custom::compact_forms::compact_form_canonical;
pub use method::Method;
pub use serializer::{message_summary, serialize, sip_summary};
pub use sdp::{
    build_answer_from_offer, build_held_sdp_from_profile, extract_codec_profile,
    rewrite_connection_and_ports, validate_sdp_body, BuildAnswerOptions, BuildHeldSdpOptions,
    CodecProfile, SdpBuildResult, SdpValidationError,
};
pub use sdp_diff::sdp_media_equivalent;
/// The session description read as a document — the only home for SDP grammar.
pub use sdp_doc::{
    canonical_rtpmap, extract_direction, extract_format_list, extract_rtpmaps, parse_origin,
    parse_sdp_body, MediaLine, SdpDirection, SdpDoc, SdpOrigin,
};
pub use parser::{SipParser, SipParserLimits};
pub use sip_str::{SharedText, SipStr};
pub use parser::custom::{hydrate_request, CustomParser};
pub use template::{
    apply_name_forms, apply_remote_target_emits, emitted_wire, EmitOpts, MessageTemplate,
    TemplateHeader, TemplateStart,
};
pub use multipart::{
    compose as compose_multipart, decompose as decompose_multipart, Composed, LocatedPart,
    MultipartError, MultipartPart,
};
pub use projection::HeaderProjection;
pub use template_match::{MatchOpts, Mismatch};
pub use deviation::{
    Automatic, CseqDeviation, CseqOp, CseqOpAt, CseqPattern, DelayedAutomatic,
};
pub use types::{
    ContactSet, CoreHeaders, InDialogRequest, InviteRequest, MessageCore, NonEmpty, NotInDialog,
    OptionalHeaders, SipHeader, SipMessage, SipRequest, SipResponse, SipResponseTagged,
};
