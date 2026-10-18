//! sip-template — a captured SIP message prepared for near-verbatim replay
//! ([`template`]), and the comparison of a received message against one
//! ([`template_match`]). Every header is read and written through
//! `sip-message`.

pub mod template;
pub mod template_match;

pub use template::{
    apply_name_forms, apply_remote_target_emits, emitted_wire, EmitOpts, MessageTemplate,
    TemplateHeader, TemplateStart,
};
pub use template_match::{MatchOpts, Mismatch};
