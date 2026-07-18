//! Scoped RFC-audit waivers. A [`WaiverScope`] waives exactly one named
//! violation on a chosen emitting party and/or message position; every other
//! finding — and the SAME rule on any other party — stays gated.
//!
//! Party attribution is a MECHANISM, not a convention: a finding is attributed
//! to the party that EMITTED the offending message, read from the recorder's
//! `from_lane` on the recorded wire entry (who sent it). A party-scoped waiver
//! filters a finding only when the emitter matches, so a finding attributed to
//! any other party — in a SUT-full lane, the SUT — is never filtered by it.
//!
//! A declared waiver that filtered nothing is an ERROR at `finish()` by default
//! (it catches a position-dependent waiver that stopped matching); a waiver
//! whose shape is legitimately conditional opts out via [`WaiverScope::conditional`].

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use layer_harness::Stamped;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{to_sip_entries, RfcFinding, SignalingNetworkEvent};

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
    /// The 1-based wire position of the offending message; `None` = any message.
    /// Scopes a waiver to one occurrence so a second one elsewhere still gates.
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

/// One recorded wire entry reduced to what attribution needs.
struct WireMsg {
    position: usize,
    /// The sender's PARTY name (resolved from its bind address), if known.
    from_party: Option<String>,
    call_id: Option<String>,
    from_tag: Option<String>,
}

/// The party name of a `from_lane` bind key (`ip:port` or `ip:port#label`),
/// resolved through the recorder's addr→name map.
fn party_of(from_lane: &Option<String>, addr_names: &HashMap<SocketAddr, String>) -> Option<String> {
    let key = from_lane.as_ref()?;
    let addr: SocketAddr = key.split('#').next()?.parse().ok()?;
    addr_names.get(&addr).filter(|n| !n.is_empty()).cloned()
}

/// Parse the recorded trace into positioned wire messages (1-based), keeping
/// each message's sender PARTY and, for a request, its dialog `(Call-ID, From-tag)`.
fn wire_messages(
    events: &[Stamped<SignalingNetworkEvent>],
    addr_names: &HashMap<SocketAddr, String>,
) -> Vec<WireMsg> {
    let parser = CustomParser::new();
    to_sip_entries(events)
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let (call_id, from_tag) = match parser.parse(&e.raw) {
                Ok(SipMessage::Request(r)) => (Some(r.call_id.clone()), r.from.tag.clone()),
                _ => (None, None),
            };
            WireMsg {
                position: i + 1,
                from_party: party_of(&e.from_lane, addr_names),
                call_id,
                from_tag,
            }
        })
        .collect()
}

/// Extract `key`'s value from an audit detail (`key` includes the `=`), reading
/// up to the next space — the `Call-ID=… from-tag=… to-tag=…` descriptor the
/// dialog rules embed.
fn detail_value(detail: &str, key: &str) -> Option<String> {
    let start = detail.find(key)? + key.len();
    let rest = &detail[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// The emitting parties and offending wire positions attributed to a finding:
/// the request(s) whose dialog `(Call-ID, From-tag)` matches the finding's
/// detail descriptor. Empty when the detail carries no descriptor (a
/// party/position-scoped waiver then never matches — it stays gated).
fn attribute(finding: &RfcFinding, msgs: &[WireMsg]) -> (HashSet<String>, HashSet<usize>) {
    let call_id = detail_value(&finding.detail, "Call-ID=");
    let from_tag = detail_value(&finding.detail, "from-tag=");
    let mut parties = HashSet::new();
    let mut positions = HashSet::new();
    if let (Some(cid), Some(ftag)) = (&call_id, &from_tag) {
        for m in msgs {
            if m.call_id.as_deref() == Some(cid) && m.from_tag.as_deref() == Some(ftag) {
                if let Some(party) = &m.from_party {
                    parties.insert(party.clone());
                }
                positions.insert(m.position);
            }
        }
    }
    (parties, positions)
}

/// Whether `w` covers `finding` given its attributed emitter parties + positions.
fn covers(w: &WaiverScope, finding: &RfcFinding, parties: &HashSet<String>, positions: &HashSet<usize>) -> bool {
    if w.rule != finding.rule {
        return false;
    }
    if let Some(p) = &w.party {
        if !parties.contains(p) {
            return false;
        }
    }
    if let Some(pos) = w.position {
        if !positions.contains(&pos) {
            return false;
        }
    }
    true
}

/// Apply the waivers to the audit findings: drop every non-advisory finding a
/// waiver covers (marking that waiver used), returning the remaining gating
/// `(lane, detail)` pairs. Attribution reads `from_lane` off the recording.
pub(super) fn apply_waivers(
    events: &[Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    addr_names: &HashMap<SocketAddr, String>,
) -> Vec<(String, String)> {
    let msgs = wire_messages(events, addr_names);
    sip_net::evaluate_rfc_findings(events)
        .into_iter()
        .filter(|f| !f.advisory)
        .filter(|f| {
            let (parties, positions) = attribute(f, &msgs);
            let mut waived = false;
            for w in waivers {
                if covers(&w.scope, f, &parties, &positions) {
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
