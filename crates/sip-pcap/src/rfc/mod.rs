//! RFC-violation detectors over an emitted flows document: the closed rule
//! vocabulary, the report model every detector fills in, and the dispatch that
//! runs them.
//!
//! A detector decides a rule off the WIRE, from what one endpoint was seen to
//! take and then emit — or fail to emit — so a claim a document makes about a
//! violation is something a tool verified rather than something an author
//! asserted. The vocabulary is closed and grows one detector at a time.
//!
//! **Vantage.** A detector reasons per ENDPOINT (`ip:port`), never per leg as
//! a whole: a leg carries both peers' traffic, and only one of them broke the
//! rule. Direction is what separates them — a message with `dst == E` is one E
//! took, a message with `src == E` is one E emitted — and the capture-time
//! order of those two facts is the evidence.
//!
//! **Repeats are not fresh events.** A message carrying `repeat_of` is
//! skipped: retransmitting a 2xx until it is ACKed is required behaviour
//! (RFC 3261 §13.3.1.4), so only the FIRST emission of a given final decides
//! a rule. The relation is bounded twice
//! ([`crate::callfacts::mark_repeats`]): by the transaction envelope, so
//! matching bytes emitted after it are a fresh emission and DO decide rules,
//! and by the CLASS, so an unreliable provisional — which retransmits on no
//! timer — states no relation at all. A platform that rings twice rang twice,
//! at four hundred milliseconds as at a minute.
//!
//! **A detector is conservative by contract.** Where the capture cannot settle
//! whether a rule was broken — an unobserved message, an unreadable header, a
//! dialog that ended before the obligation came due — the population counter
//! records the occasion and NO hit is reported. Under-reporting is a smaller
//! defect than charging compliant traffic.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::doc::FlowsDoc;

#[cfg(test)]
mod ack;
mod adapter;
#[cfg(test)]
mod cancel;
mod census;
#[cfg(test)]
mod prack;
mod sut;
#[cfg(test)]
mod testkit;

pub use census::{Census, LocatedHit, ReadFailure, RuleTally};
pub use sut::{Side, SutSet};

/// The rule vocabulary and its evidence live ONCE, in `rfc-rules` (issue 29);
/// this module is the capture ADAPTER over them. The census runs the
/// [`RfcRule::WIRE`] subset — the rules whose corpus numbers back the pivot
/// §11.1 contract — plus the candidates a sweep names to take their baseline
/// ([`scan_with`]).
pub use rfc_rules::{Evidence, RuleId as RfcRule};

/// Which side of the captured deployment an endpoint sits on, as the GROUP
/// TOPOLOGY tells it — which endpoint the call's legs cross, see
/// [`roles_of_group`].
///
/// Evidence, never a deployment statement: a consumer that knows which
/// addresses are the system under test attributes the side from that, and a
/// capture where the crossed box is a peer's reads `Platform` here regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointRole {
    /// Seen on more than one leg of the call: the box the other legs cross.
    Platform,
    /// Seen on exactly one leg of a call whose legs a platform endpoint joins.
    Peer,
    /// A single-leg call shows no crossing, so neither side is attributable.
    Undetermined,
}

impl EndpointRole {
    /// The report token.
    pub fn token(self) -> &'static str {
        match self {
            EndpointRole::Platform => "platform",
            EndpointRole::Peer => "peer",
            EndpointRole::Undetermined => "undetermined",
        }
    }
}

/// One violation, located in the corpus and anchored on the messages that
/// prove it.
///
/// The head is what every rule states: WHO is charged, on which dialog and in
/// which transaction. The rule's own proof rides [`Evidence`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub rule: RfcRule,
    /// Index into `doc.groups`.
    pub group: usize,
    /// Index into `doc.legs`.
    pub leg: usize,
    pub call_id: String,
    /// The endpoint charged with the violation, `ip:port` — the one that
    /// emitted the offending message, or that owed the one never emitted.
    pub emitter: String,
    pub emitter_role: EndpointRole,
    /// The side a stated SUT set places the emitter on ([`SutSet`]) —
    /// absent when no set was stated, so a report taken without one is
    /// unchanged.
    #[serde(default, skip_serializing_if = "Side::is_unattributed")]
    pub side: Side,
    /// The endpoint on the other side of the obligation: where the offending
    /// message went, or who was owed the message that never came.
    pub taker: String,
    /// CSeq NUMBER of the INVITE transaction — the key the evidence shares.
    pub cseq: u32,
    /// The emitter forwarded a message it had itself been sent rather than
    /// originating the behaviour, so the rule the far endpoint broke is
    /// already counted against that endpoint. A relayed hit is a weaker
    /// finding than an originated one.
    pub relayed: bool,
    #[serde(flatten)]
    pub evidence: Evidence,
}

/// What one document's bytes prove, and the population that proof sits in.
///
/// The denominators matter as much as the hits: a violation count means one
/// thing against ten cancelled INVITEs and another against ten thousand.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub hits: Vec<Hit>,
    /// Per rule token: the occasions the rule could have been broken, and the
    /// subset of those the capture could actually DECIDE. `hits ⊆ decided ⊆
    /// occasions`, so the cost of the detectors' conservatism is visible
    /// rather than implied.
    pub population: BTreeMap<&'static str, Population>,
}

/// One rule's denominators inside one document.
#[derive(Debug, Clone, Copy, Default)]
pub struct Population {
    pub occasions: u64,
    pub decided: u64,
}

impl Scan {
    /// Record one occasion of `rule`, and whether the capture decided it.
    fn count(&mut self, rule: RfcRule, decided: bool) {
        let p = self.population.entry(rule.token()).or_default();
        p.occasions += 1;
        if decided {
            p.decided += 1;
        }
    }
}

/// Every violation the document's own bytes prove, with its denominators —
/// the [`RfcRule::WIRE`] vocabulary.
pub fn scan(doc: &FlowsDoc) -> Scan {
    scan_with(doc, &[])
}

/// [`scan`] with `candidates` run beside the WIRE vocabulary: rules with a body
/// but no corpus numbers yet, whose baseline THIS sweep takes. A candidate's
/// hits are tallied under its own token and never join the WIRE contract by
/// being counted.
pub fn scan_with(doc: &FlowsDoc, candidates: &[RfcRule]) -> Scan {
    scan_sut(doc, candidates, None)
}

/// [`scan_with`], every hit also placed on a side by `sut` when one is
/// stated. The topology role is computed regardless: the two answer
/// different questions and a reader compares them.
pub fn scan_sut(doc: &FlowsDoc, candidates: &[RfcRule], sut: Option<&SutSet>) -> Scan {
    let mut out = Scan::default();
    let span = Span::of(doc);
    for (gi, group) in doc.groups.iter().enumerate() {
        let roles = roles_of_group(doc, &group.legs);
        for &li in &group.legs {
            let Some(leg) = doc.legs.get(li) else { continue };
            let at = Site { leg, leg_index: li, group: gi, roles: &roles, span: &span, sut };
            adapter::detect(&at, &mut out, candidates);
        }
    }
    out
}

/// The IP an `ip:port` endpoint token names (`#label` suffix and port left
/// off), or `None` where the token is not a socket address.
pub(crate) fn endpoint_ip(endpoint: &str) -> Option<std::net::IpAddr> {
    rfc_rules::wire::endpoint_addr(endpoint).map(|s| s.ip())
}

/// How long the RECORDING ran, as the whole document tells it.
///
/// A rule whose offence is an absence needs this and cannot get it from one
/// leg: a leg can fall silent at the very message that creates the obligation
/// while the capture goes on recording every other wire for minutes, and the
/// difference between those two facts is the difference between a violation
/// and a truncation.
pub(crate) struct Span {
    /// The last capture timestamp anywhere in the document.
    pub last_us: u64,
    /// Per endpoint `ip:port`: the last timestamp at which the recording
    /// carries a message that endpoint sent or took.
    pub endpoint_last_us: BTreeMap<String, u64>,
}

impl Span {
    fn of(doc: &FlowsDoc) -> Span {
        let mut span = Span { last_us: 0, endpoint_last_us: BTreeMap::new() };
        for leg in &doc.legs {
            for msg in &leg.msgs {
                span.last_us = span.last_us.max(msg.ts_us);
                for endpoint in [&msg.src, &msg.dst] {
                    let at = span.endpoint_last_us.entry(endpoint.clone()).or_default();
                    *at = (*at).max(msg.ts_us);
                }
            }
        }
        span
    }
}

/// The violations alone — [`scan`] without its denominators.
pub fn detect(doc: &FlowsDoc) -> Vec<Hit> {
    scan(doc).hits
}

/// One leg, located in its document — what every detector is handed.
pub(crate) struct Site<'a> {
    pub leg: &'a crate::doc::LegJson,
    pub leg_index: usize,
    pub group: usize,
    pub roles: &'a BTreeMap<String, EndpointRole>,
    /// What the whole capture says about how long it kept recording.
    pub span: &'a Span,
    /// The stated SUT set, when the scan has one.
    pub sut: Option<&'a SutSet>,
}

impl Site<'_> {
    /// The head of a hit at this site, charged to `emitter` and owed to
    /// `taker`.
    pub(crate) fn hit(
        &self,
        rule: RfcRule,
        emitter: &str,
        taker: &str,
        cseq: u32,
        relayed: bool,
        evidence: Evidence,
    ) -> Hit {
        Hit {
            rule,
            group: self.group,
            leg: self.leg_index,
            call_id: self.leg.call_id.clone(),
            emitter: emitter.to_string(),
            emitter_role: self.roles.get(emitter).copied().unwrap_or(EndpointRole::Undetermined),
            side: self.sut.map_or(Side::Unattributed, |s| s.side_of(emitter)),
            taker: taker.to_string(),
            cseq,
            relayed,
            evidence,
        }
    }
}

/// Endpoint → role across ONE call group: an endpoint on several of the
/// group's legs is the box those legs cross; with such an endpoint present the
/// others are its peers; a group whose legs share no endpoint attributes
/// neither side.
fn roles_of_group(doc: &FlowsDoc, legs: &[usize]) -> BTreeMap<String, EndpointRole> {
    let mut on_legs: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    for &li in legs {
        let Some(leg) = doc.legs.get(li) else { continue };
        for hop in &leg.hops {
            on_legs.entry(hop.a.as_str()).or_default().insert(li);
            on_legs.entry(hop.b.as_str()).or_default().insert(li);
        }
    }
    let any_platform = on_legs.values().any(|l| l.len() > 1);
    on_legs
        .into_iter()
        .map(|(ep, l)| {
            let role = match (l.len() > 1, any_platform) {
                (true, _) => EndpointRole::Platform,
                (false, true) => EndpointRole::Peer,
                (false, false) => EndpointRole::Undetermined,
            };
            (ep.to_string(), role)
        })
        .collect()
}
