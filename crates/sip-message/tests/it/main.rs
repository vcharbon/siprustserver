//! Every `sip-message` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=sip-message) & binary(it)' <module>::`.

mod abnf_fuzz;
mod ack_2xx_from_invite;
mod ack_non_2xx_in_dialog;
mod compliance_matrix;
mod contact_set;
mod dialog_contact;
mod draft_round_trip;
mod generated;
mod generators;
mod header_cardinality;
mod header_registry_typing;
mod header_round_trip;
mod host_grammar;
mod invalid_request;
mod lazy_headers;
mod line_endings;
mod no_panic_lane;
mod parse_refusal;
mod parser;
mod parser_body_bound;
mod parser_extraction;
mod parser_fold;
mod parser_line_folding;
mod parser_response_totag;
mod parser_smoke;
mod recorded_routes;
#[cfg(feature = "rvoip-oracle")]
mod rvoip_parity;
mod sdp_answer;
mod sdp_form;
mod sdp_offer_answer_grammar;
mod sdp_utils;
mod sipfrag;
mod uri_grammar;
mod via_lower_hops;
mod via_sent_by;
mod write_line_safety;
