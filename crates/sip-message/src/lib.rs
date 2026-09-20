//! sip-message — the pure SIP message layer: parse / serialize /
//! strict-validate plus read/rewrite helpers. No async, no I/O, no clock —
//! a pure leaf crate.
//!
//! This is the ONLY crate that extracts SIP headers/messages. Lenient
//! raw-datagram scanning: [`sniff`]; strict pre-parse classifiers:
//! [`preparse`]; parsed-header access: the typed read surface on
//! [`SipRequest`] / [`SipResponse`] ([`header`] values, [`HeaderName`]);
//! construction: [`draft`] and [`generators`].

mod access;
pub mod capture;
pub mod draft;
pub mod error;
pub mod header;
pub mod method;
pub mod parser;
pub mod sip_str;
pub mod types;

pub mod deviation;
pub mod emergency;
pub mod generators;
pub mod hops;
pub mod multipart;
pub mod param_codec;
pub mod payload;
pub mod preparse;
pub mod projection;
pub mod remote_target;
pub mod sdp;
pub mod sdp_diff;
pub mod sdp_doc;
pub mod serializer;
pub mod sipfrag;
pub mod sniff;
pub mod template;
pub mod template_match;
pub mod trace_sample;

/// The body and image type the public surface hands out and takes back.
pub use bytes::Bytes;
pub use deviation::{Automatic, CseqDeviation, CseqOp, CseqOpAt, CseqPattern, DelayedAutomatic};
pub use error::SipParseError;
/// Header identity and the one ownership table (ADR-0025).
pub use header::{canonical_header_items, header_forms_equivalent, HeaderClass, HeaderName};
pub use method::Method;
pub use multipart::{
    compose as compose_multipart, decompose as decompose_multipart, sdp_range, Composed,
    LocatedPart, MultipartError, MultipartPart,
};
/// RFC 3261 §7.3.3 compact-form expansion, probed one name at a time.
pub use parser::custom::compact_forms::compact_form_canonical;
pub use parser::custom::{hydrate_request, CustomParser};
pub use parser::{Framing, SipParser, SipParserLimits};
pub use projection::HeaderProjection;
pub use sdp::{
    build_answer_from_offer, build_held_sdp_from_profile, extract_codec_profile,
    rewrite_connection_and_ports, validate_sdp_body, BuildAnswerOptions, BuildHeldSdpOptions,
    CodecProfile, SdpBuildResult, SdpValidationError,
};
pub use sdp_diff::sdp_media_equivalent;
/// The session description read as a document — the only home for SDP grammar.
pub use sdp_doc::{
    canonical_rtpmap, extract_direction, extract_format_list, extract_rtpmaps, parse_origin,
    parse_sdp_body, reoffer_continuing, MediaLine, SdpDirection, SdpDoc, SdpOrigin,
};
pub use serializer::{message_summary, serialize, sip_summary};
pub use sip_str::{SharedText, SipStr};
pub use template::{
    apply_name_forms, apply_remote_target_emits, emitted_wire, EmitOpts, MessageTemplate,
    TemplateHeader, TemplateStart,
};
pub use template_match::{MatchOpts, Mismatch};
pub use types::{
    ContactSet, CoreHeaders, InDialogRequest, InviteRequest, MessageCore, NonEmpty, NotInDialog,
    OptionalHeaders, SipHeader, SipMessage, SipRequest, SipResponse, SipResponseTagged,
};
