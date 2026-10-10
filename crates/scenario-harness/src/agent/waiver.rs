//! Scoped RFC-audit waivers. A [`WaiverScope`] waives exactly one named
//! violation on a chosen emitting party and/or message position; every other
//! finding — and the SAME rule on any other party — stays gated.
//!
//! Party attribution is a MECHANISM, not a convention: it references the
//! offending message STRUCTURALLY. A finding carries
//! [`RfcFinding::offending`](sip_net::RfcFinding) — the wire-entry index of the
//! exact message that violated the rule — and the party is the finding's
//! `charged` bind (who sent it, or for an omission who owed it), else that
//! entry's `from_lane` party. A party-scoped waiver filters a finding only
//! when the party matches, so a finding attributed to any other party — in a
//! SUT-full lane, the SUT — is never filtered by it.
//!
//! A finding charged to the system under test (a party the harness bound as
//! SUT, or a finding no simulated party is charged with) is covered only by a
//! waiver that names that SUT party: a party-less waiver
//! ([`Harness::allow_violation`](super::Harness::allow_violation),
//! [`WaiverScope::rule`] alone) covers simulated-party findings only.
//!
//! A position-scoped waiver needs the finding's `offending` entry: a finding
//! that does not carry one stays gated and such a waiver goes unused-loud.
//!
//! A declared waiver that filtered nothing is an ERROR at `finish()` by default
//! (it catches a position-dependent waiver that stopped matching); a waiver
//! whose shape is legitimately conditional opts out via [`WaiverScope::conditional`].

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;

use layer_harness::Stamped;
use sip_net::{audit_wire_entries, RecordedSipEntry, RfcFinding, SignalingNetworkEvent};

/// A data-constructible RFC-audit waiver scoped to `(rule, party, position)`.
/// Scenario-file portable (plain data). Without a party it covers the rule on
/// any SIMULATED party; a finding charged to the SUT needs
/// [`on_party`](Self::on_party) naming the SUT. Narrow it further with
/// [`at_position`](Self::at_position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiverScope {
    /// The audit rule id waived (e.g. `"cseq-in-dialog-order"`).
    pub rule: String,
    /// The EMITTING party (a bind/lane name) whose non-compliance is waived;
    /// `None` = any simulated party (never the SUT). A finding emitted by a
    /// different party is never filtered by this waiver.
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
    /// A rule-scoped waiver (any simulated party, any message), unused-loud by
    /// default.
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
    /// addr→first-name map (one logical endpoint per socket); `sut` holds the
    /// names of the lanes the harness bound as SUT.
    AddrNames { names: &'a HashMap<SocketAddr, String>, sut: &'a HashSet<String> },
    /// Load lane: the `from_lane` sub-lane suffix (`ip:port#name` → `name`). Mux
    /// sockets carry SEVERAL logical legs, so the addr→first-name map would
    /// mis-attribute every co-socketed leg to the first bind — the sub-lane key
    /// is the leg's true identity. Every sub-lane is a simulated party.
    SubLane,
}

impl Attribution<'_> {
    /// Whether `party` is a simulated party: named, and not a SUT lane. An
    /// unattributed finding is not known to be a simulated party's.
    fn is_simulated(&self, party: Option<&str>) -> bool {
        match (party, self) {
            (None, _) => false,
            (Some(p), Attribution::AddrNames { sut, .. }) => !sut.contains(p),
            (Some(_), Attribution::SubLane) => true,
        }
    }
}

/// The party name of a `from_lane` bind key under an [`Attribution`].
fn party_of(from_lane: &Option<String>, attr: &Attribution) -> Option<String> {
    let key = from_lane.as_ref()?;
    match attr {
        Attribution::AddrNames { names: map, .. } => {
            let addr: SocketAddr = key.split('#').next()?.parse().ok()?;
            map.get(&addr).filter(|n| !n.is_empty()).cloned()
        }
        Attribution::SubLane => {
            key.split_once('#').map(|(_, name)| name.to_string()).filter(|n| !n.is_empty())
        }
    }
}

/// The party a finding holds responsible: its `charged` bind where the rule
/// names one — the emitter of the offending message, or for an omission the
/// party that owed the message never sent — else the `from_lane` party of its
/// offending wire entry. `None` when the rule pinpoints neither.
fn finding_party(
    f: &RfcFinding,
    entries: &[RecordedSipEntry],
    attr: &Attribution,
) -> Option<String> {
    let entry = f.offending.and_then(|idx| entries.get(idx.checked_sub(1)?));
    if let Some(charged) = &f.charged {
        return party_of(&Some(charged_lane(charged, entry)?), attr);
    }
    party_of(&entry?.from_lane, attr)
}

/// The bind key a `charged` value names. A rule judged at the taker charges
/// the sender by the bare source address it saw; that address is the
/// offending entry's sender or taker, whose lane carries the sub-lane a mux
/// socket shares between legs. An address naming two different legs at the
/// entry's two ends (legs on one socket talking to each other) names no lane:
/// `None`, so the finding stays unattributed.
fn charged_lane(charged: &str, entry: Option<&RecordedSipEntry>) -> Option<String> {
    if charged.contains('#') {
        return Some(charged.to_string());
    }
    let mut ends = entry
        .into_iter()
        .flat_map(|e| [&e.from_lane, &e.to_lane])
        .flatten()
        .filter(|lane| lane.split('#').next() == Some(charged));
    match (ends.next(), ends.next()) {
        (Some(a), Some(b)) if a != b => None,
        (Some(a), _) => Some(a.clone()),
        (None, _) => Some(charged.to_string()),
    }
}

/// Whether `w` covers `finding`, charged to `party`: the rule matches, the
/// position (when scoped) is the offending entry, and the party is the one the
/// waiver names — or, for a party-less waiver, a simulated party.
fn covers(w: &WaiverScope, finding: &RfcFinding, party: Option<&str>, attr: &Attribution) -> bool {
    if w.rule != finding.rule {
        return false;
    }
    if let Some(pos) = w.position {
        if finding.offending != Some(pos) {
            return false;
        }
    }
    match &w.party {
        Some(named) => party == Some(named.as_str()),
        None => attr.is_simulated(party),
    }
}

/// The findings a waiver set lets through, and the SUT findings it covered.
pub(crate) struct Waived<T> {
    /// The non-advisory findings no waiver covers.
    pub gating: Vec<T>,
    /// Per rule, how many findings charged to the SUT a waiver covered.
    pub sut: BTreeMap<String, usize>,
}

/// The non-advisory findings that SURVIVE the waivers (marking each covering
/// waiver used) — the shared core of both apply variants. Attribution
/// references each finding's `offending` wire-entry index directly. Takes the
/// PRE-EVALUATED suite output so a caller that already ran
/// `sip_net::evaluate_rfc_findings` (the finish gate) never runs it twice.
fn survivors(
    findings: &[RfcFinding],
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    attr: &Attribution,
) -> Waived<(RfcFinding, Option<String>)> {
    waive(findings, &audit_wire_entries(events), waivers, attr)
}

/// [`survivors`] over the audit's wire entries.
fn waive(
    findings: &[RfcFinding],
    entries: &[RecordedSipEntry],
    waivers: &[WaiverState],
    attr: &Attribution,
) -> Waived<(RfcFinding, Option<String>)> {
    let mut out = Waived { gating: Vec::new(), sut: BTreeMap::new() };
    for f in findings.iter().filter(|f| !f.advisory) {
        let party = finding_party(f, entries, attr);
        let mut waived = false;
        for w in waivers {
            if covers(&w.scope, f, party.as_deref(), attr) {
                w.used.set(true);
                waived = true;
            }
        }
        if !waived {
            out.gating.push((f.clone(), party));
        } else if !attr.is_simulated(party.as_deref()) {
            *out.sut.entry(f.rule.clone()).or_default() += 1;
        }
    }
    out
}

/// Apply the waivers to the PRE-EVALUATED audit findings: drop every
/// non-advisory finding a waiver covers (marking that waiver used), returning
/// each remaining gating finding with the party it charges, and the waived SUT
/// findings per rule — the functional-lane (addr→name) attribution.
pub(super) fn apply_waivers(
    findings: &[RfcFinding],
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    names: &HashMap<SocketAddr, String>,
    sut: &HashSet<String>,
) -> Waived<(RfcFinding, Option<String>)> {
    survivors(findings, events, waivers, &Attribution::AddrNames { names, sut })
}

/// The finding-preserving apply variant (ADR-0024 §6): the surviving structured
/// [`RfcFinding`]s, so the load driver can bucket them by rule id (not by "first
/// error seen"). Marks each covering waiver used (read `WaiverState::used`
/// after, per campaign). `attr` is the lane's party attribution. Evaluates the
/// suite itself — the load lane has no shared report to reuse a set from.
pub(crate) fn apply_waivers_findings(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    attr: &Attribution,
) -> Vec<RfcFinding> {
    let findings = sip_net::evaluate_rfc_findings(events);
    survivors(&findings, events, waivers, attr).gating.into_iter().map(|(f, _)| f).collect()
}

/// The declared waivers that filtered NOTHING and are not `conditional` — an
/// error at `finish()`.
pub(super) fn unused_waivers(waivers: &[WaiverState]) -> Vec<&WaiverScope> {
    waivers
        .iter()
        .filter(|w| {
            crate::dialogs::is_partyless_dialog_waiver(&w.scope)
                || (!w.used.get() && !w.scope.conditional)
        })
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
            charged: None,
        }
    }

    const RULE: &str = "cseq-in-dialog-order";

    fn covered(
        w: &WaiverScope,
        f: &RfcFinding,
        entries: &[RecordedSipEntry],
        attr: &Attribution,
    ) -> bool {
        covers(w, f, finding_party(f, entries, attr).as_deref(), attr)
    }

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
        assert!(covered(&w, &bob, &entries, &attr), "covers bob's finding");
        assert!(
            !covered(&w, &bob2, &entries, &attr),
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
        let attr = Attribution::AddrNames { names: &map, sut: &HashSet::new() };
        let w = WaiverScope::rule(RULE, "bob").on_party("bob");
        assert!(covered(&w, &finding(RULE, 1), &entries, &attr));
        assert!(
            covered(&w, &finding(RULE, 2), &entries, &attr),
            "addr map wrongly covers the sibling (both resolve to the first bind)",
        );
    }

    /// A party-scoped waiver on a DIFFERENT party (a SUT lane, an unnamed key)
    /// never covers — the finding stays gated.
    #[test]
    fn sublane_party_scope_gates_other_parties() {
        let entries = vec![entry("10.0.0.9:5070#bob")];
        let attr = Attribution::SubLane;
        assert!(!covered(
            &WaiverScope::rule(RULE, "proxy").on_party("proxy"),
            &finding(RULE, 1),
            &entries,
            &attr,
        ));
    }

    /// An omission is charged to the party that OWED the missing message, not
    /// to the sender of the message it is anchored on: a waiver naming the
    /// owing party covers it, one naming the anchor's sender does not.
    #[test]
    fn an_omission_is_attributed_to_the_party_that_owed_it() {
        let entries = vec![entry("10.0.0.9:5070")]; // the anchor: the B2BUA's provisional
        let mut map = HashMap::new();
        map.insert("10.0.0.9:5070".parse().unwrap(), "b2bua".to_string());
        map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
        let attr = Attribution::AddrNames { names: &map, sut: &HashSet::new() };
        let owed = RfcFinding { charged: Some("10.0.0.1:5060".into()), ..finding(RULE, 1) };
        let alice = WaiverScope::rule(RULE, "alice never PRACKs").on_party("alice");
        let b2bua = WaiverScope::rule(RULE, "wrong party").on_party("b2bua");
        assert!(covered(&alice, &owed, &entries, &attr), "the owing party's waiver covers it");
        assert!(!covered(&b2bua, &owed, &entries, &attr), "the anchor's sender owed nothing");
    }

    /// A rule judged at the taker charges the sender by the bare source
    /// address it saw: on the load lane that address resolves through the
    /// offending entry's sub-lane at either end, so the sender's waiver
    /// covers it, an omission owed by the entry's taker is the taker's, and a
    /// co-socketed sibling's waiver covers neither.
    #[test]
    fn sublane_bare_charged_address_resolves_through_the_offending_entry() {
        let mut to_bob = entry("10.0.0.1:5060#alice");
        to_bob.to_lane = Some("10.0.0.9:5070#bob".into());
        let mut from_bob2 = entry("10.0.0.9:5070#bob2");
        from_bob2.to_lane = Some("10.0.0.1:5060#alice".into());
        let entries = vec![to_bob, from_bob2];
        let attr = Attribution::SubLane;
        let by_sender = RfcFinding { charged: Some("10.0.0.1:5060".into()), ..finding(RULE, 1) };
        let by_taker = RfcFinding { charged: Some("10.0.0.9:5070".into()), ..finding(RULE, 1) };
        let by_bob2 = RfcFinding { charged: Some("10.0.0.9:5070".into()), ..finding(RULE, 2) };
        let alice = WaiverScope::rule(RULE, "alice").on_party("alice");
        let bob = WaiverScope::rule(RULE, "bob").on_party("bob");
        let bob2 = WaiverScope::rule(RULE, "bob2").on_party("bob2");
        assert!(covered(&alice, &by_sender, &entries, &attr), "the sender's waiver covers it");
        assert!(!covered(&bob, &by_sender, &entries, &attr), "the taker owed nothing");
        assert!(covered(&bob, &by_taker, &entries, &attr), "the owing taker's waiver covers it");
        assert!(!covered(&bob2, &by_taker, &entries, &attr), "a co-socketed sibling never does");
        assert!(covered(&bob2, &by_bob2, &entries, &attr), "the sibling's own entry is its own");
        assert!(!covered(&bob, &by_bob2, &entries, &attr), "and never bob's");
    }

    /// Two legs on one socket talking to each other: a bare charged address
    /// names both ends of the offending entry, so it resolves to no party and
    /// no party-scoped waiver covers the finding.
    #[test]
    fn sublane_bare_charged_address_naming_both_ends_stays_gated() {
        let mut ok = entry("127.0.0.1:5060#bob"); // bob's 200, alice never ACKs it
        ok.to_lane = Some("127.0.0.1:5060#alice".into());
        let entries = vec![ok];
        let attr = Attribution::SubLane;
        let unacked = RfcFinding { charged: Some("127.0.0.1:5060".into()), ..finding(RULE, 1) };
        let alice = WaiverScope::rule(RULE, "alice").on_party("alice");
        let bob = WaiverScope::rule(RULE, "bob").on_party("bob");
        assert!(!covered(&bob, &unacked, &entries, &attr), "the sender is not the charged party");
        assert!(!covered(&alice, &unacked, &entries, &attr), "nor is it known to be the taker");
    }

    /// A rule-only waiver covers by rule alone (a plain rule-name filter)
    /// regardless of attribution.
    #[test]
    fn rule_only_covers_by_rule_regardless_of_party() {
        let entries = vec![entry("10.0.0.9:5070#bob")];
        let attr = Attribution::SubLane;
        assert!(covered(&WaiverScope::rule(RULE, "coarse"), &finding(RULE, 1), &entries, &attr,));
        // A different rule is never covered.
        assert!(!covered(
            &WaiverScope::rule("rfc3261.other", "coarse"),
            &finding(RULE, 1),
            &entries,
            &attr,
        ));
    }

    /// A party-less waiver covers a simulated party's finding, never one
    /// charged to a lane the harness bound as SUT nor an unattributed one; a
    /// waiver naming the SUT lane covers the SUT's.
    #[test]
    fn a_party_less_waiver_never_covers_the_sut() {
        let entries = vec![entry("10.0.0.9:5070"), entry("10.0.0.1:5060")];
        let mut map = HashMap::new();
        map.insert("10.0.0.9:5070".parse().unwrap(), "sut".to_string());
        map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
        let sut = HashSet::from(["sut".to_string()]);
        let attr = Attribution::AddrNames { names: &map, sut: &sut };
        let coarse = WaiverScope::rule(RULE, "coarse");
        let by_sut = finding(RULE, 1);
        let by_alice = finding(RULE, 2);
        let unattributed = RfcFinding { offending: None, ..finding(RULE, 1) };
        assert!(covered(&coarse, &by_alice, &entries, &attr), "a simulated party's finding");
        assert!(!covered(&coarse, &by_sut, &entries, &attr), "never the SUT's");
        assert!(!covered(&coarse, &unattributed, &entries, &attr), "nor an unattributed one");
        let named = WaiverScope::rule(RULE, "accepted").on_party("sut");
        assert!(covered(&named, &by_sut, &entries, &attr), "a waiver naming the SUT covers it");
    }

    /// The waived SUT findings are counted per rule; a waived simulated
    /// party's finding is not.
    #[test]
    fn waived_sut_findings_are_counted_per_rule() {
        let entries = vec![entry("10.0.0.9:5070"), entry("10.0.0.1:5060")];
        let mut map = HashMap::new();
        map.insert("10.0.0.9:5070".parse().unwrap(), "sut".to_string());
        map.insert("10.0.0.1:5060".parse().unwrap(), "alice".to_string());
        let sut = HashSet::from(["sut".to_string()]);
        let attr = Attribution::AddrNames { names: &map, sut: &sut };
        let waivers = [
            WaiverState::new(WaiverScope::rule(RULE, "accepted").on_party("sut")),
            WaiverState::new(WaiverScope::rule(RULE, "alice")),
        ];
        let findings = [finding(RULE, 1), finding(RULE, 1), finding(RULE, 2)];
        let out = waive(&findings, &entries, &waivers, &attr);
        assert!(out.gating.is_empty(), "every finding is waived");
        assert_eq!(out.sut, BTreeMap::from([(RULE.to_string(), 2)]));
    }
}
