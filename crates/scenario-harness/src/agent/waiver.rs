//! Scoped RFC-audit waivers. A [`WaiverScope`] waives exactly one named
//! violation on a chosen emitting party and/or message position; every other
//! finding — and the SAME rule on any other party — stays gated.
//!
//! Party attribution is a MECHANISM, not a convention: it references the
//! offending message STRUCTURALLY. A finding carries
//! [`RfcFinding::offending`](sip_net_testkit::RfcFinding) — the wire-entry index of the
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
use sip_net_testkit::{audit_wire_entries, RecordedSipEntry, RfcFinding, SignalingNetworkEvent};

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
    /// A dialog-verdict waiver's method: only verdicts on requests of it are
    /// covered. `None` = any method.
    pub method: Option<String>,
    /// A dialog-verdict waiver's start: only verdicts recorded from this
    /// instant on are covered. Set by a live run, never by a scenario file.
    pub since: Option<tokio::time::Instant>,
    /// A dialog-verdict waiver that covers at most one verdict per dialog.
    pub once_per_dialog: bool,
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
            method: None,
            since: None,
            once_per_dialog: false,
            justification: justification.into(),
        }
    }

    /// Narrow a dialog-verdict waiver to the verdicts on `method` requests.
    pub fn on_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    /// Narrow a dialog-verdict waiver to the verdicts recorded from now on,
    /// at most one per dialog.
    pub fn from_now_once_per_dialog(mut self) -> Self {
        self.since = Some(tokio::time::Instant::now());
        self.once_per_dialog = true;
        self
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
    /// Per rule, how many findings charged to a simulated party a waiver
    /// covered.
    pub simulated: BTreeMap<String, usize>,
}

/// The non-advisory findings that SURVIVE the waivers (marking each covering
/// waiver used) — the shared core of both apply variants. Attribution
/// references each finding's `offending` wire-entry index directly. Takes the
/// PRE-EVALUATED suite output so a caller that already ran
/// `sip_net_testkit::evaluate_rfc_findings` (the finish gate) never runs it twice.
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
    let mut out = Waived { gating: Vec::new(), sut: BTreeMap::new(), simulated: BTreeMap::new() };
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
        } else if attr.is_simulated(party.as_deref()) {
            *out.simulated.entry(f.rule.clone()).or_default() += 1;
        } else {
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
    let findings = sip_net_testkit::evaluate_rfc_findings(events);
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
mod tests;
