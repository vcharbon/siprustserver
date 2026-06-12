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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::Stamped;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParserLimits;

use crate::contracts::{CrossMessageAuditRule, PeerAuditRule, SignalingNetworkEvent};
use crate::types::{all_ua_roles, UaRole};

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

// ---------------------------------------------------------------------------
// Shared role-aware evaluator
// ---------------------------------------------------------------------------

/// One RFC-suite finding over a recorded trace, post subject dispatch: the rule
/// only fired against a bind whose declared roles intersect the rule's
/// `subject()`. `advisory` mirrors the rule's `force_advisory()` — an advisory
/// finding is surfaced (reports, the e2e findings table) but never gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfcFinding {
    /// The rule id (e.g. `rfc3261.proxy100WithinT100ms`).
    pub rule: String,
    /// The bind (lane) the finding is attributed to.
    pub lane: String,
    /// Human-readable explanation.
    pub detail: String,
    /// `true` ⇒ informational only; `false` ⇒ a gating violation.
    pub advisory: bool,
}

/// The declared roles of every bind in a recorded trace, reconstructed from the
/// `BindAcquire` summaries (a bind that declared none defaults to all roles).
/// This is what lets a consumer apply subject dispatch with no harness-private
/// state — the trace itself carries the role tags.
pub fn bind_roles_of(
    events: &[Stamped<SignalingNetworkEvent>],
) -> HashMap<String, HashSet<UaRole>> {
    let mut roles = HashMap::new();
    for s in events {
        if let SignalingNetworkEvent::BindAcquire { bind_key, summary } = &s.event {
            roles.insert(bind_key.clone(), summary.roles.clone());
        }
    }
    roles
}

/// Run the FULL default RFC suite (peer + cross-message rules) over a recorded
/// trace, applying **subject dispatch** per finding: a finding is kept only when
/// its rule's `subject()` intersects the originating bind's declared roles
/// (default = all roles, so this only narrows when roles were declared at
/// `bind_udp`). This is THE single evaluator — the harness hard gate panics on
/// the non-advisory subset, and the report projection lists the whole set with
/// its `advisory` tags — so the gate and the report can never disagree on
/// which endpoint a rule applies to.
pub fn evaluate_rfc_findings(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<RfcFinding> {
    let roles = bind_roles_of(events);
    let roles_of =
        |bind: &str| roles.get(bind).cloned().unwrap_or_else(all_ua_roles);
    let subject_hits = |subject: &HashSet<UaRole>, bind: &str| {
        let r = roles_of(bind);
        subject.iter().any(|s| r.contains(s))
    };

    let mut findings = Vec::new();

    // Cross-message rules — one pass over the whole channel each.
    for rule in rfc_cross_message_rules() {
        let subject = rule.subject();
        for (bind, detail) in rule.check(events) {
            if subject_hits(&subject, &bind) {
                findings.push(RfcFinding {
                    rule: rule.name().to_string(),
                    lane: bind,
                    detail,
                    advisory: rule.force_advisory(),
                });
            }
        }
    }

    // Peer rules — per-bind slice.
    let peer_rules = rfc_peer_rules();
    if !peer_rules.is_empty() {
        let mut binds: Vec<String> = Vec::new();
        for s in events {
            let bk = s.event.bind_key();
            if !binds.iter().any(|b| b == bk) {
                binds.push(bk.clone());
            }
        }
        for bind in &binds {
            let slice: Vec<Stamped<SignalingNetworkEvent>> = events
                .iter()
                .filter(|s| s.event.bind_key() == bind)
                .cloned()
                .collect();
            for rule in &peer_rules {
                if !subject_hits(&rule.subject(), bind) {
                    continue;
                }
                for detail in rule.check(&slice, bind) {
                    findings.push(RfcFinding {
                        rule: rule.name().to_string(),
                        lane: bind.clone(),
                        detail,
                        advisory: rule.force_advisory(),
                    });
                }
            }
        }
    }
    findings
}
