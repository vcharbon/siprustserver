//! sip-message — the pure SIP message layer: parse / serialize /
//! strict-validate plus read/rewrite helpers. No async, no I/O, no clock —
//! a pure leaf crate.
//!
//! This is the ONLY crate that extracts SIP headers/messages. Lenient
//! raw-datagram scanning: [`sniff`]; strict pre-parse classifiers:
//! [`message_helpers::preparse`]; parsed-header access:
//! [`message_helpers`]; construction: [`generators`].

pub mod error;
pub mod method;
pub mod types;
pub mod parser;

pub mod serializer;
pub mod sdp;
pub mod generators;
pub mod message_helpers;
pub mod sipfrag;
pub mod sniff;
pub mod deviation;
pub mod remote_target;
pub mod template;
pub mod template_match;

pub use error::SipParseError;
pub use method::Method;
pub use serializer::{message_summary, serialize, serialize_request_parts, serialize_response_parts, sip_summary};
pub use sdp::{
    build_answer_from_offer, build_held_sdp_from_profile, extract_codec_profile, validate_sdp_body,
    BuildAnswerOptions, BuildHeldSdpOptions, CodecProfile, SdpBuildResult, SdpValidationError,
};
pub use parser::{SipParser, SipParserLimits};
pub use parser::custom::{hydrate_request, hydrate_response, CustomParser};
pub use template::{
    apply_name_forms, apply_remote_target_emits, EmitOpts, HeaderClass, MessageTemplate,
    TemplateHeader, TemplateStart,
};
pub use template_match::{MatchOpts, Mismatch};
pub use deviation::{
    Automatic, CseqDeviation, CseqOp, CseqOpAt, CseqPattern, DelayedAutomatic,
};
pub use types::{
    Contact, ContactSet, CSeq, InDialogRequest, InviteRequest, NameAddr, NonEmpty, NotInDialog,
    OptionalHeaders, Params, ParamValue, Rack, ReferTo, Replaces, RequestUri, SipHeader,
    SipMessage, SipRequest, SipResponse, SipResponseTagged, TypedHeader, Uri, Via,
};
