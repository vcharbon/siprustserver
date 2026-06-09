//! **TEST-ONLY.** RFC 3261 / 3262 / 3264 audit rules over the recorded
//! signaling layer — the Rust port of `tests/harness/rules/rfc/*` from the
//! TypeScript reference server.
//!
//! These run at layer close (when wired into [`ScopedAuditOptions`](crate::ScopedAuditOptions))
//! or directly over a channel snapshot via [`rfc_cross_message_rules`] /
//! [`rfc_peer_rules`]. They flag on-wire protocol invariants that a real
//! UAC/UAS enforces but the test UAs (which answer whatever they are handed,
//! regardless of CSeq / tags / Route) do not — so the recording itself, not the
//! per-step `expect`, is where those invariants are checked. Wiring the full set
//! into the default options gives every harness run the same "post-run all
//! clean" RFC check the live SIPp endpoints apply in endurance.
//!
//! Module layout mirrors the TS source files:
//!   - [`cseq`]          — the original RFC 3261 §8/§12/§13 CSeq family (cross).
//!   - [`starter_peer`]  — generic per-message peer validators (TS `starter-peer-rules.ts`).
//!   - [`rfc3261_peer`] / [`rfc3262_peer`] / [`rfc3264_peer`] — per-RFC peer rules.
//!   - [`cross_generic`] — generic per-dialog cross-message rules (TS `cross-message-rules.ts`).
//!   - [`rfc3261_cross`] / [`rfc3262_cross`] / [`rfc3264_cross`] — per-RFC cross rules.
//!
//! Shared helpers (ports of the TS `_*.ts` helpers):
//!   - [`dialog_model`]    — `_dialog-model.ts`: per-agent dialog state + per-dialog projector.
//!   - [`txn_correlation`] — `_transaction-correlation.ts`: top-Via-branch request/response index.
//!   - [`offer_answer`]    — `_offer-answer.ts`: SDP offer/answer lift (over `sip_message::sdp`).

use std::sync::Arc;

use sip_message::parser::custom::CustomParser;
use sip_message::SipParserLimits;

use crate::contracts::{CrossMessageAuditRule, PeerAuditRule};

/// The **lenient** parser the audit layer uses (port of the TS
/// `createCustomParser({ wireGrammar: false })`). The default [`CustomParser`]
/// runs the ADR-0007 strict-grammar gates (`wire_grammar = true`), which *reject*
/// a malformed-but-on-the-wire message (e.g. a `Via` branch lacking the magic
/// cookie) before a rule could ever see it — so the grammar rules would silently
/// never fire. Auditing the recorded bytes requires parsing them leniently, the
/// same way a permissive peer on the wire would, so the rule (not the parser)
/// is what flags the violation.
pub(crate) fn lenient_parser() -> CustomParser {
    CustomParser::with_limits(SipParserLimits { wire_grammar: false, ..Default::default() })
}

pub mod cseq;

// Shared helpers.
pub mod dialog_model;
pub mod offer_answer;
pub mod txn_correlation;

// Per-message peer rules.
pub mod rfc3261_peer;
pub mod rfc3262_peer;
pub mod rfc3264_peer;
pub mod starter_peer;

// Per-dialog / cross-message rules.
pub mod cross_generic;
pub mod rfc3261_cross;
pub mod rfc3262_cross;
pub mod rfc3264_cross;

// Keep the original public name exported for back-compat.
pub use cseq::CSeqInDialogOrderRule;

/// The full default set of **per-message peer** RFC rules every test harness
/// installs by default (see [`crate::with_all_contracts`] and the
/// `scenario-harness` `Harness`). Each rule runs against a single bind's events
/// and only when its `subject()` intersects that bind's declared roles.
pub fn rfc_peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    let mut v: Vec<Arc<dyn PeerAuditRule>> = Vec::new();
    v.extend(starter_peer::peer_rules());
    v.extend(rfc3261_peer::peer_rules());
    v.extend(rfc3262_peer::peer_rules());
    v.extend(rfc3264_peer::peer_rules());
    v
}

/// The full default set of **cross-message** RFC rules every test harness
/// installs by default. One pass over the whole recorded channel at layer
/// close; each finding carries its originating bind so subject dispatch and the
/// hard gate can filter it.
pub fn rfc_cross_message_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    let mut v: Vec<Arc<dyn CrossMessageAuditRule>> = Vec::new();
    v.extend(cseq::cross_rules());
    v.extend(cross_generic::cross_rules());
    v.extend(rfc3261_cross::cross_rules());
    v.extend(rfc3262_cross::cross_rules());
    v.extend(rfc3264_cross::cross_rules());
    v
}
