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
pub(crate) struct WaiverState {
    pub scope: WaiverScope,
    pub used: Cell<bool>,
}

impl WaiverState {
    pub(crate) fn new(scope: WaiverScope) -> Self {
        WaiverState { scope, used: Cell::new(false) }
    }
}

/// How a finding's OFFENDING message maps to an emitting party (ADR-0024 §6).
/// Attribution always references the offending wire-entry's `from_lane`
/// structurally — never a detail string — differing only in how a bind key
/// resolves to a party name.
pub(crate) enum Attribution<'a> {
    /// Functional lane: resolve the `from_lane` addr through the recorder's
    /// addr→first-name map (one logical endpoint per socket).
    AddrNames(&'a HashMap<SocketAddr, String>),
    /// Load lane: the `from_lane` sub-lane suffix (`ip:port#name` → `name`). Mux
    /// sockets carry SEVERAL logical legs, so the addr→first-name map would
    /// mis-attribute every co-socketed leg to the first bind — the sub-lane key
    /// is the leg's true identity.
    SubLane,
}

/// The party name of a `from_lane` bind key under an [`Attribution`].
fn party_of(from_lane: &Option<String>, attr: &Attribution) -> Option<String> {
    let key = from_lane.as_ref()?;
    match attr {
        Attribution::AddrNames(map) => {
            let addr: SocketAddr = key.split('#').next()?.parse().ok()?;
            map.get(&addr).filter(|n| !n.is_empty()).cloned()
        }
        Attribution::SubLane => key
            .split_once('#')
            .map(|(_, name)| name.to_string())
            .filter(|n| !n.is_empty()),
    }
}

/// The emitting party of a finding — the `from_lane` party of its offending
/// wire entry. `None` when the rule does not pinpoint the entry.
fn finding_party(
    f: &RfcFinding,
    entries: &[RecordedSipEntry],
    attr: &Attribution,
) -> Option<String> {
    let idx = f.offending?;
    let entry = entries.get(idx.checked_sub(1)?)?;
    party_of(&entry.from_lane, attr)
}

/// Whether `w` covers `finding`, referencing the offending message directly.
fn covers(
    w: &WaiverScope,
    finding: &RfcFinding,
    entries: &[RecordedSipEntry],
    attr: &Attribution,
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
        if finding_party(finding, entries, attr).as_ref() != Some(party) {
            return false;
        }
    }
    true
}

/// The non-advisory findings that SURVIVE the waivers (marking each covering
/// waiver used) — the shared core of both apply variants. Attribution
/// references each finding's `offending` wire-entry index directly.
fn survivors(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    attr: &Attribution,
) -> Vec<RfcFinding> {
    let entries = audit_wire_entries(events);
    sip_net::evaluate_rfc_findings(events)
        .into_iter()
        .filter(|f| !f.advisory)
        .filter(|f| {
            let mut waived = false;
            for w in waivers {
                if covers(&w.scope, f, &entries, attr) {
                    w.used.set(true);
                    waived = true;
                }
            }
            !waived
        })
        .collect()
}

/// Apply the waivers to the audit findings: drop every non-advisory finding a
/// waiver covers (marking that waiver used), returning the remaining gating
/// `(lane, detail)` pairs — the functional-lane (addr→name) attribution.
pub(super) fn apply_waivers(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    addr_names: &HashMap<SocketAddr, String>,
) -> Vec<(String, String)> {
    survivors(events, waivers, &Attribution::AddrNames(addr_names))
        .into_iter()
        .map(|f| (f.lane, f.detail))
        .collect()
}

/// The finding-preserving apply variant (ADR-0024 §6): the surviving structured
/// [`RfcFinding`]s, so the load driver can bucket them by rule id (not by "first
/// error seen"). Marks each covering waiver used (read `WaiverState::used`
/// after, per campaign). `attr` is the lane's party attribution.
pub(crate) fn apply_waivers_findings(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    attr: &Attribution,
) -> Vec<RfcFinding> {
    survivors(events, waivers, attr)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(from_lane: &str) -> RecordedSipEntry {
        RecordedSipEntry {
            from: "10.0.0.9:5070".parse().unwrap(),
            to: "10.0.0.1:5060".parse().unwrap(),
            raw: b"OPTIONS sip:x@h SIP/2.0\r\n\r\n".to_vec(),
            sent_ms: 0,
            received_ms: Some(1),
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: Some(from_lane.to_string()),
            to_lane: None,
            seq: 0,
        }
    }

    fn finding(rule: &str, offending: usize) -> RfcFinding {
        RfcFinding {
            rule: rule.into(),
            lane: "x".into(),
            detail: "d".into(),
            advisory: false,
            offending: Some(offending),
        }
    }

    const RULE: &str = "rfc3261.cseqInDialogOrder";

    /// ADR-0024 §6 load-lane specific: sub-lane party attribution. Two logical
    /// legs share ONE mux socket; a party-scoped waiver covers ONLY the leg it
    /// names, never its co-socketed sibling — because attribution resolves the
    /// `ip:port#name` sub-lane key, not the addr.
    #[test]
    fn sublane_party_scope_isolates_co_socketed_siblings() {
        let entries = vec![entry("10.0.0.9:5070#bob"), entry("10.0.0.9:5070#bob2")];
        let bob = finding(RULE, 1); // offending = entry #1 (bob)
        let bob2 = finding(RULE, 2); // offending = entry #2 (bob2, same socket)
        let w = WaiverScope::rule(RULE, "only bob").on_party("bob");
        let attr = Attribution::SubLane;
        assert!(covers(&w, &bob, &entries, &attr), "covers bob's finding");
        assert!(
            !covers(&w, &bob2, &entries, &attr),
            "does NOT cover the co-socketed sibling bob2's finding",
        );
    }

    /// The plain addr→first-name map (the functional attribution) would
    /// mis-attribute BOTH co-socketed legs to the first bind — the exact hazard
    /// the load lane's sub-lane resolution avoids.
    #[test]
    fn addr_map_mis_attributes_co_socketed_legs() {
        let entries = vec![entry("10.0.0.9:5070#bob"), entry("10.0.0.9:5070#bob2")];
        let mut map = HashMap::new();
        map.insert("10.0.0.9:5070".parse().unwrap(), "bob".to_string());
        let attr = Attribution::AddrNames(&map);
        let w = WaiverScope::rule(RULE, "bob").on_party("bob");
        assert!(covers(&w, &finding(RULE, 1), &entries, &attr));
        assert!(
            covers(&w, &finding(RULE, 2), &entries, &attr),
            "addr map wrongly covers the sibling (both resolve to the first bind)",
        );
    }

    /// A party-scoped waiver on a DIFFERENT party (a SUT lane, an unnamed key)
    /// never covers — the finding stays gated.
    #[test]
    fn sublane_party_scope_gates_other_parties() {
        let entries = vec![entry("10.0.0.9:5070#bob")];
        let attr = Attribution::SubLane;
        assert!(!covers(
            &WaiverScope::rule(RULE, "proxy").on_party("proxy"),
            &finding(RULE, 1),
            &entries,
            &attr,
        ));
    }

    /// A rule-only waiver covers by rule alone (byte-for-byte the old
    /// `HashSet<String>` filter) regardless of attribution.
    #[test]
    fn rule_only_covers_by_rule_regardless_of_party() {
        let entries = vec![entry("10.0.0.9:5070#bob")];
        let attr = Attribution::SubLane;
        assert!(covers(
            &WaiverScope::rule(RULE, "coarse"),
            &finding(RULE, 1),
            &entries,
            &attr,
        ));
        // A different rule is never covered.
        assert!(!covers(
            &WaiverScope::rule("rfc3261.other", "coarse"),
            &finding(RULE, 1),
            &entries,
            &attr,
        ));
    }
}
