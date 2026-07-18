//! Scoped RFC-audit waivers. A [`WaiverScope`] waives exactly one named
//! violation on a chosen emitting party and/or message position; every other
//! finding — and the SAME rule on any other party — stays gated.
//!
//! Party attribution is a MECHANISM, not a convention: it references the
//! offending message STRUCTURALLY. A finding carries
//! [`RfcFinding::offending`](sip_net::RfcFinding) — the wire-entry index of the
//! exact message that violated the rule — and the emitter is that entry's
//! `from_lane` party (who sent it). A party-scoped waiver filters a finding only
//! when the emitter matches, so a finding attributed to any other party — in a
//! SUT-full lane, the SUT — is never filtered by it.
//!
//! A finding whose rule does NOT populate `offending` is unattributable: a
//! party- or position-scoped waiver never matches it (it stays gated) and such a
//! waiver goes unused-loud. Only the coarse rule-only waiver
//! ([`Harness::allow_violation`](super::Harness::allow_violation)) covers it.
//!
//! A declared waiver that filtered nothing is an ERROR at `finish()` by default
//! (it catches a position-dependent waiver that stopped matching); a waiver
//! whose shape is legitimately conditional opts out via [`WaiverScope::conditional`].

use std::cell::Cell;
use std::collections::HashMap;
use std::net::SocketAddr;

use layer_harness::Stamped;
use sip_net::{audit_wire_entries, RecordedSipEntry, RfcFinding, SignalingNetworkEvent};

/// A data-constructible RFC-audit waiver scoped to `(rule, party, position)`.
/// Scenario-file portable (plain data). Coarse by default (any party, any
/// message); narrow it with [`on_party`](Self::on_party) / [`at_position`](Self::at_position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiverScope {
    /// The audit rule id waived (e.g. `"rfc3261.cseqInDialogOrder"`).
    pub rule: String,
    /// The EMITTING party (a bind/lane name) whose non-compliance is waived;
    /// `None` = any party. A finding emitted by a different party is never
    /// filtered by this waiver.
    pub party: Option<String>,
    /// The 1-based wire-entry index of the OFFENDING message (into the audit
    /// view, `Harness::wire_entries()`); `None` = any message. Scopes a waiver to
    /// one occurrence so a second one elsewhere still gates. Matched against the
    /// finding's `offending` index, so it pins the exact offender — a compliant
    /// neighbour's position does NOT cover a violation.
    pub position: Option<usize>,
    /// When `true`, a waiver that filters nothing is NOT an error at `finish()`
    /// (a legitimately conditional shape). Default `false` (unused ⇒ loud).
    pub conditional: bool,
    /// Human-readable reason (surfaced on registration / in the report).
    pub justification: String,
}

impl WaiverScope {
    /// A rule-scoped waiver (any party, any message), unused-loud by default.
    pub fn rule(rule: impl Into<String>, justification: impl Into<String>) -> Self {
        WaiverScope {
            rule: rule.into(),
            party: None,
            position: None,
            conditional: false,
            justification: justification.into(),
        }
    }

    /// Narrow to the emitting party (a bind/lane name).
    pub fn on_party(mut self, party: impl Into<String>) -> Self {
        self.party = Some(party.into());
        self
    }

    /// Narrow to the offending message's 1-based wire position.
    pub fn at_position(mut self, position: usize) -> Self {
        self.position = Some(position);
        self
    }

    /// Opt out of the unused-waiver error (a legitimately conditional shape).
    pub fn conditional(mut self) -> Self {
        self.conditional = true;
        self
    }
}

/// A registered waiver plus whether it has filtered a finding this run.
pub(super) struct WaiverState {
    pub scope: WaiverScope,
    pub used: Cell<bool>,
}

impl WaiverState {
    pub(super) fn new(scope: WaiverScope) -> Self {
        WaiverState { scope, used: Cell::new(false) }
    }
}

/// The party name of a `from_lane` bind key (`ip:port` or `ip:port#label`),
/// resolved through the recorder's addr→name map.
fn party_of(from_lane: &Option<String>, addr_names: &HashMap<SocketAddr, String>) -> Option<String> {
    let key = from_lane.as_ref()?;
    let addr: SocketAddr = key.split('#').next()?.parse().ok()?;
    addr_names.get(&addr).filter(|n| !n.is_empty()).cloned()
}

/// The emitting party of a finding — the `from_lane` party of its offending
/// wire entry. `None` when the rule does not pinpoint the entry.
fn finding_party(
    f: &RfcFinding,
    entries: &[RecordedSipEntry],
    addr_names: &HashMap<SocketAddr, String>,
) -> Option<String> {
    let idx = f.offending?;
    let entry = entries.get(idx.checked_sub(1)?)?;
    party_of(&entry.from_lane, addr_names)
}

/// Whether `w` covers `finding`, referencing the offending message directly.
fn covers(
    w: &WaiverScope,
    finding: &RfcFinding,
    entries: &[RecordedSipEntry],
    addr_names: &HashMap<SocketAddr, String>,
) -> bool {
    if w.rule != finding.rule {
        return false;
    }
    // A coarse rule-only waiver covers by rule regardless of attribution
    // (backward-compatible `allow_violation`).
    if w.party.is_none() && w.position.is_none() {
        return true;
    }
    // A party/position-scoped waiver needs a pinpointed offending message; a
    // finding that does not carry one is unattributable and stays gated.
    let Some(idx) = finding.offending else {
        return false;
    };
    if let Some(pos) = w.position {
        if pos != idx {
            return false;
        }
    }
    if let Some(party) = &w.party {
        if finding_party(finding, entries, addr_names).as_ref() != Some(party) {
            return false;
        }
    }
    true
}

/// Apply the waivers to the audit findings: drop every non-advisory finding a
/// waiver covers (marking that waiver used), returning the remaining gating
/// `(lane, detail)` pairs. Attribution references each finding's `offending`
/// wire-entry index directly (no detail-string parsing).
pub(super) fn apply_waivers(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    addr_names: &HashMap<SocketAddr, String>,
) -> Vec<(String, String)> {
    let entries = audit_wire_entries(events);
    sip_net::evaluate_rfc_findings(events)
        .into_iter()
        .filter(|f| !f.advisory)
        .filter(|f| {
            let mut waived = false;
            for w in waivers {
                if covers(&w.scope, f, &entries, addr_names) {
                    w.used.set(true);
                    waived = true;
                }
            }
            !waived
        })
        .map(|f| (f.lane, f.detail))
        .collect()
}

/// The declared waivers that filtered NOTHING and are not `conditional` — an
/// error at `finish()`.
pub(super) fn unused_waivers(waivers: &[WaiverState]) -> Vec<&WaiverScope> {
    waivers
        .iter()
        .filter(|w| !w.used.get() && !w.scope.conditional)
        .map(|w| &w.scope)
        .collect()
}
