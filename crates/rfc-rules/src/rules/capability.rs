//! What an endpoint says about what it accepts — the `Allow` / `Supported` /
//! `Require` / `Unsupported` / `Accept` family, and the rejections RFC 3261 §8.2
//! spells in terms of them. EIGHT obligations, each read at one endpoint:
//!
//!   - [`AllowSupportedOnInvite`] (§13.2.1 / §20.37): a re-INVITE and an INVITE
//!     2xx advertise what their sender accepts.
//!   - [`UnsupportedMethod405Allow`] (§8.2.1): an unrecognised METHOD is
//!     answered 405 with `Allow`.
//!   - [`UnsupportedExtension420`] (§8.2.2): an unsupported `Require` tag is
//!     answered 420 with `Unsupported`.
//!   - [`Unsupported415Accepts`] (§8.2.3): a 415 names the formats its sender
//!     does accept.
//!   - [`UnsupportedExtension421`] (§21.4.15): a 421 lists what it demands.
//!   - [`OptionsResponseEchoes`] (§11.2): a 2xx to OPTIONS describes the
//!     sender's capabilities.
//!   - [`NoRequireOnCancelOrAck`] (§8.2.2.3): a CANCEL, and the ACK of a
//!     non-2xx final, demand no extension at all.
//!
//! **A rejection is judged at its TAKER**, because that is the endpoint the
//! obligation runs against: the request arrived at it, and what it answered is
//! its own emission. The occasion is keyed by `(taker, Call-ID, top-Via
//! branch)` — RFC 3261 §17's transaction identity — so a §17.2.3 retransmission
//! of the request neither opens a second occasion nor replaces the answer
//! already recorded, and a retransmitted final is the same answer again.
//!
//! **Facts beyond the wire model come from `Msg::head` via `sip_message::sniff`
//! — never parsed here.** Where the unreadable header is what would make the
//! occasion EXIST (the `Require` tags on a request), there is no occasion at
//! all; where the occasion stands on wire-model facts and only its verdict
//! needs the bytes, the occasion is `Undecidable`.

use std::collections::{btree_map::Entry, BTreeMap};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::branch::{BranchKey, BranchReading};
use super::Obligation;

/// How long an OPEN observation must keep running past a request before "it was
/// never answered" reads as an absence rather than a truncation.
///
/// One second. RFC 3261 §17.2.2 has a non-INVITE UAS answer from the core, and
/// §17.2.1 starts the INVITE UAS's retransmission ladder at T1 (500 ms) — a
/// recording that ran a full second past the request and carries no answer
/// shows silence, not a cut. In a CLOSED observation it collapses.
pub const ANSWER_WINDOW_US: u64 = 1_000_000;

/// Methods a modern UA recognises — anything else is the §8.2.1 occasion.
const RECOGNISED_METHODS: &[&str] = &[
    "INVITE",
    "ACK",
    "BYE",
    "CANCEL",
    "OPTIONS",
    "REGISTER",
    "PRACK",
    "UPDATE",
    "INFO",
    "REFER",
    "SUBSCRIBE",
    "NOTIFY",
    "MESSAGE",
    "PUBLISH",
];

/// Option tags a modern UA recognises — a `Require` naming anything else is the
/// §8.2.2 occasion.
const RECOGNISED_OPTION_TAGS: &[&str] =
    &["100rel", "timer", "replaces", "gruu", "path", "outbound", "eventlist", "sec-agree"];

/// The transaction one rejection obligation rides, as its TAKER names it:
/// RFC 3261 §17's `(Call-ID, top-Via branch)` at one endpoint.
type TxnKey<'a> = (&'a str, &'a str, &'a str);

/// The key `msg` opens at the endpoint that TOOK it, or `None` where the
/// vantage carried no branch to name the transaction by.
fn taken_key(msg: &Msg) -> Option<TxnKey<'_>> {
    let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty())?;
    Some((msg.dst.as_str(), msg.call_id.as_str(), branch))
}

/// The key `msg` answers at the endpoint that SENT it — the mirror of
/// [`taken_key`], since a response rides its request's branch.
fn sent_key(msg: &Msg) -> Option<TxnKey<'_>> {
    let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty())?;
    Some((msg.src.as_str(), msg.call_id.as_str(), branch))
}

/// What one endpoint answered on one transaction: the FIRST final it sent, and
/// how many rows of the header the correct rejection owes it carried.
#[derive(Clone, Copy)]
struct Answer {
    status: u16,
    /// `None` where the vantage carried no header block for the answer.
    listed_rows: Option<u32>,
}

/// **RFC 3261 §13.2.1 / §20.37 — a re-INVITE and an INVITE 2xx advertise Allow
/// and Supported.** The peer negotiates `Require` against what the sender says
/// it accepts, so a re-offer or an answer that carries neither header hides the
/// sender's capability set. A B2BUA that strips them on re-offers is the case
/// this catches; the test UA never inspects them.
///
/// The occasion is ONE such message the endpoint took. Charges the endpoint
/// that sent it.
///
/// **Three exemptions, each because the message is not a re-offer.** The FIRST
/// INVITE this vantage saw the endpoint take on a call opens the negotiation. A
/// later INVITE repeating that branch is a §17.2.3 retransmission of it. And a
/// dialog-ESTABLISHING INVITE — one carrying no To tag — is a fresh attempt on
/// the call (the §22.2 retry after a 401), never an in-dialog re-offer.
pub struct AllowSupportedOnInvite;

impl Obligation for AllowSupportedOnInvite {
    fn id(&self) -> RuleId {
        RuleId::AllowSupportedOnInvite
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // The first INVITE branch each endpoint took per call — the one that
        // opened the negotiation, and the branch its retransmissions repeat.
        let mut opened: BTreeMap<(&str, &str), &str> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let status = match &msg.kind {
                Kind::Request { method } => {
                    if !method.eq_ignore_ascii_case("INVITE") {
                        continue;
                    }
                    let branch = msg.via_branch.as_deref().unwrap_or_default();
                    match opened.entry((msg.dst.as_str(), msg.call_id.as_str())) {
                        Entry::Vacant(e) => {
                            e.insert(branch);
                            continue; // the INVITE that opened the negotiation
                        }
                        // A same-branch repeat is a §17.2.3 retransmission of
                        // that INVITE, exempt exactly as the original is.
                        Entry::Occupied(e) if *e.get() == branch => continue,
                        Entry::Occupied(_) => {}
                    }
                    // A second To-tag-less INVITE is another attempt at
                    // establishing the dialog (§22.2 auth retry), not a re-offer.
                    if msg.to_tag.as_deref().is_none_or(str::is_empty) {
                        continue;
                    }
                    0
                }
                Kind::Response { status } => {
                    if !(200..300).contains(status)
                        || !msg.cseq_method.eq_ignore_ascii_case("INVITE")
                    {
                        continue;
                    }
                    *status
                }
            };
            let finding = |decision| Finding {
                rule: RuleId::AllowSupportedOnInvite,
                emitter: msg.src.to_string(),
                taker: msg.dst.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let missing: Vec<String> = ["Allow", "Supported"]
                .into_iter()
                .filter(|name| !sniff::has_header(head, name))
                .map(str::to_string)
                .collect();
            if missing.is_empty() {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::CapabilitiesNotAdvertised {
                capability_msg: mi,
                capability_hop: msg.hop,
                capability_ts_us: msg.at_us,
                missing,
                status,
            })));
        }
        out
    }
}

/// **§8.2.1 — an unrecognised request method is answered 405 with `Allow`.** A
/// UAS handed a verb it does not implement tells the sender so, and names the
/// verbs it does implement, so the UAC can pick another rather than retry into
/// silence. A real UAS rejects the unknown method; the test UA answers whatever
/// it is handed, which is what makes this worth checking on the recording.
///
/// The occasion is ONE unrecognised request the endpoint TOOK, keyed by its
/// transaction. Charges that endpoint. The answer that discharges it is a 405
/// carrying at least one `Allow` row — any other final, or none at all, is the
/// miss. A request the vantage carried no branch for names no transaction and
/// is no occasion; an answer whose header block the vantage lost is
/// `Undecidable`.
pub struct UnsupportedMethod405Allow;

impl Obligation for UnsupportedMethod405Allow {
    fn id(&self) -> RuleId {
        RuleId::UnsupportedMethod405Allow
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let taken = |msg: &Msg| match &msg.kind {
            Kind::Request { method } => {
                !RECOGNISED_METHODS.iter().any(|m| m.eq_ignore_ascii_case(method))
            }
            Kind::Response { .. } => false,
        };
        rejections(wire, RuleId::UnsupportedMethod405Allow, taken, "Allow", |occasion, answer| {
            let discharged =
                answer.is_some_and(|a| a.status == 405 && a.listed_rows.unwrap_or(0) > 0);
            (
                discharged,
                Evidence::RejectionNotIssued {
                    rejection_msg: occasion.msg,
                    rejection_hop: occasion.hop,
                    rejection_ts_us: occasion.ts_us,
                    method: occasion.method.to_string(),
                    branch: occasion.branch.to_string(),
                    unsupported_tags: Vec::new(),
                    answered_status: answer.map_or(0, |a| a.status),
                    listed_rows: answer.and_then(|a| a.listed_rows).unwrap_or(0),
                },
            )
        })
    }
}

/// **§8.2.2 — an unsupported `Require` tag is answered 420 with
/// `Unsupported`.** A UAS that cannot honour a mandatory extension refuses the
/// request and names the tags it refused, so the UAC can retry without them.
/// A real UAS refuses; the test UA ignores `Require` entirely.
///
/// The occasion is ONE request the endpoint TOOK whose `Require` names a tag
/// outside the recognised set, keyed by its transaction. Charges that endpoint.
/// A 420 carrying at least one `Unsupported` row discharges it. A request whose
/// header block the vantage lost is NOT an occasion — the `Require` tags are
/// what would make it one.
pub struct UnsupportedExtension420;

impl Obligation for UnsupportedExtension420 {
    fn id(&self) -> RuleId {
        RuleId::UnsupportedExtension420
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let taken = |msg: &Msg| {
            matches!(msg.kind, Kind::Request { .. }) && !unsupported_require(msg).is_empty()
        };
        rejections(
            wire,
            RuleId::UnsupportedExtension420,
            taken,
            "Unsupported",
            |occasion, answer| {
                let discharged =
                    answer.is_some_and(|a| a.status == 420 && a.listed_rows.unwrap_or(0) > 0);
                (
                    discharged,
                    Evidence::RejectionNotIssued {
                        rejection_msg: occasion.msg,
                        rejection_hop: occasion.hop,
                        rejection_ts_us: occasion.ts_us,
                        method: occasion.method.to_string(),
                        branch: occasion.branch.to_string(),
                        unsupported_tags: unsupported_require(occasion.request),
                        answered_status: answer.map_or(0, |a| a.status),
                        listed_rows: answer.and_then(|a| a.listed_rows).unwrap_or(0),
                    },
                )
            },
        )
    }
}

/// The `Require` option tags `msg` states that no modern UA recognises,
/// lower-cased for comparison — the §8.2.2 occasion, and the list a 420 owes in
/// `Unsupported`. Empty where the header is absent, names nothing unrecognised,
/// or the vantage carried no header block.
fn unsupported_require(msg: &Msg) -> Vec<String> {
    msg.head
        .as_deref()
        .map(|head| {
            sniff::option_tags(head, "Require")
                .into_iter()
                .map(|t| t.to_ascii_lowercase())
                .filter(|t| !RECOGNISED_OPTION_TAGS.contains(&t.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// **§8.2.3 — a 415 names the formats its sender does accept.** Rejecting a
/// body without saying what would have been acceptable leaves the UAC nothing
/// to retry with, so §8.2.3 has the 415 carry `Accept`, `Accept-Encoding` or
/// `Accept-Language`. A real UAS guides the retry; the test UA does not.
///
/// The occasion is ONE 415 the endpoint SENT. Charges it. Any ONE of the three
/// headers meets the obligation. A §17.2.1 retransmission of that 415 is the
/// same answer again, not a second occasion.
pub struct Unsupported415Accepts;

impl Obligation for Unsupported415Accepts {
    fn id(&self) -> RuleId {
        RuleId::Unsupported415Accepts
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        sent_response_owes(
            wire,
            RuleId::Unsupported415Accepts,
            |status| status == 415,
            &["Accept", "Accept-Encoding", "Accept-Language"],
        )
    }
}

/// **§21.4.15 — a 421 lists the extensions it demands.** A 421 without
/// `Require` tells the UAC an extension is needed but not which, so there is
/// nothing to retry with. A real UAC reads the list and retries; the test UA
/// does not.
///
/// The occasion is ONE 421 the endpoint SENT. Charges it. A `Require` naming at
/// least one option tag meets it — an empty header states no demand.
pub struct UnsupportedExtension421;

impl Obligation for UnsupportedExtension421 {
    fn id(&self) -> RuleId {
        RuleId::UnsupportedExtension421
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        sent_response_owes(
            wire,
            RuleId::UnsupportedExtension421,
            |status| status == 421,
            &["Require"],
        )
    }
}

/// **§11.2 — a 2xx to OPTIONS describes the sender's capabilities.** OPTIONS
/// asks what the peer supports, so its 2xx answers with the same `Allow` /
/// `Supported` / `Accept` an INVITE response would carry; a bare 200 answers
/// the query with nothing.
///
/// The occasion is ONE 2xx the endpoint SENT to an OPTIONS its transaction
/// carried — the request is required, so a 2xx whose OPTIONS this vantage never
/// saw settles nothing and is not an occasion. Charges the sender. Any ONE of
/// the three headers meets it.
pub struct OptionsResponseEchoes;

impl Obligation for OptionsResponseEchoes {
    fn id(&self) -> RuleId {
        RuleId::OptionsResponseEchoes
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // The OPTIONS transactions each endpoint took — a 2xx is judged only on
        // one of them.
        let mut asked: BTreeMap<TxnKey<'_>, ()> = BTreeMap::new();
        let mut answered: BTreeMap<TxnKey<'_>, ()> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("OPTIONS") => {
                    if let Some(key) = taken_key(msg) {
                        asked.insert(key, ());
                    }
                }
                Kind::Response { status } if (200..300).contains(status) => {
                    if !msg.cseq_method.eq_ignore_ascii_case("OPTIONS") {
                        continue;
                    }
                    let Some(key) = sent_key(msg) else { continue };
                    if !asked.contains_key(&key) || answered.insert(key, ()).is_some() {
                        continue; // no OPTIONS at this vantage, or the same answer again
                    }
                    out.push(response_finding(
                        RuleId::OptionsResponseEchoes,
                        mi,
                        msg,
                        &["Allow", "Supported", "Accept"],
                    ));
                }
                _ => {}
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The two walks the family shares
// ---------------------------------------------------------------------------

/// One request an endpoint took that owes a specific rejection.
struct Rejectable<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    method: &'a str,
    branch: &'a str,
    emitter: &'a str,
    taker: &'a str,
    request: &'a Msg,
}

/// The §8.2 walk: pair every request `owes` selects with the FIRST final its
/// taker answered on that transaction, then let `judge` say whether the answer
/// discharged the obligation and what the evidence is.
///
/// The first FINAL, not the first response: a 100 Trying answers nothing about
/// the rejection, and reading it as the answer would charge a UAS that went on
/// to reject correctly.
fn rejections<'a>(
    wire: &WireView<'a>,
    rule: RuleId,
    owes: impl Fn(&Msg) -> bool,
    owed_header: &str,
    judge: impl Fn(&Rejectable<'a>, Option<Answer>) -> (bool, Evidence),
) -> Vec<Finding> {
    let mut occasions: Vec<Rejectable<'a>> = Vec::new();
    let mut opened: BTreeMap<TxnKey<'a>, usize> = BTreeMap::new();
    let mut answers: BTreeMap<TxnKey<'a>, Answer> = BTreeMap::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        match &msg.kind {
            Kind::Request { method } => {
                if !owes(msg) {
                    continue;
                }
                let Some(key) = taken_key(msg) else { continue };
                if let Entry::Vacant(e) = opened.entry(key) {
                    e.insert(occasions.len());
                    occasions.push(Rejectable {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        cseq: msg.cseq,
                        method: method.as_str(),
                        branch: key.2,
                        emitter: msg.dst.as_str(),
                        taker: msg.src.as_str(),
                        request: msg,
                    });
                }
            }
            Kind::Response { status } if *status >= 200 => {
                let Some(key) = sent_key(msg) else { continue };
                if !opened.contains_key(&key) {
                    continue;
                }
                answers.entry(key).or_insert(Answer {
                    status: *status,
                    listed_rows: msg
                        .head
                        .as_deref()
                        .map(|head| sniff::header_values(head, owed_header).len() as u32),
                });
            }
            Kind::Response { .. } => {}
        }
    }
    occasions
        .iter()
        .map(|occasion| {
            let key = (occasion.emitter, occasion.request.call_id.as_str(), occasion.branch);
            let answer = answers.get(&key).copied();
            let finding = |decision| Finding {
                rule,
                emitter: occasion.emitter.to_string(),
                taker: occasion.taker.to_string(),
                cseq: occasion.cseq,
                relayed: false,
                anchor: occasion.msg,
                decision,
            };
            match answer {
                Some(Answer { listed_rows: None, .. }) => {
                    finding(Decision::Undecidable("no header block at this vantage"))
                }
                None if !wire.obs.absence_decidable(occasion.ts_us, ANSWER_WINDOW_US) => {
                    finding(Decision::Undecidable(
                        "the observation stopped inside the window — \
                                           truncation, not silence",
                    ))
                }
                answer => match judge(occasion, answer) {
                    (true, _) => finding(Decision::Compliant),
                    (false, evidence) => finding(Decision::Violated(evidence)),
                },
            }
        })
        .collect()
}

/// The single-response walk: every response an endpoint SENT whose status
/// `owes` selects must carry at least one of `owed`. A retransmission of that
/// same status on the same transaction is the same answer, not a new occasion.
fn sent_response_owes(
    wire: &WireView<'_>,
    rule: RuleId,
    owes: impl Fn(u16) -> bool,
    owed: &[&str],
) -> Vec<Finding> {
    let mut judged: BTreeMap<(TxnKey<'_>, u16), ()> = BTreeMap::new();
    let mut out = Vec::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        let Kind::Response { status } = &msg.kind else { continue };
        if !owes(*status) {
            continue;
        }
        // A branchless response names no transaction; its own status still
        // keys the repeat collapse at this endpoint's call.
        let key = sent_key(msg).unwrap_or((msg.src.as_str(), msg.call_id.as_str(), ""));
        if judged.insert((key, *status), ()).is_some() {
            continue;
        }
        out.push(response_finding(rule, mi, msg, owed));
    }
    out
}

/// One response's verdict on the headers its status owes: carrying ANY of them
/// meets the obligation, carrying none is the miss, and a lost header block
/// settles nothing.
fn response_finding(rule: RuleId, mi: usize, msg: &Msg, owed: &[&str]) -> Finding {
    let finding = |decision| Finding {
        rule,
        emitter: msg.src.to_string(),
        taker: msg.dst.to_string(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    };
    let Some(head) = msg.head.as_deref() else {
        return finding(Decision::Undecidable("no header block at this vantage"));
    };
    let carried = owed.iter().any(|name| match *name {
        // §21.4.15 asks the 421 to LIST what it demands: the header is there
        // but empty states no demand at all.
        "Require" => !sniff::option_tags(head, "Require").is_empty(),
        name => sniff::has_header(head, name),
    });
    if carried {
        return finding(Decision::Compliant);
    }
    finding(Decision::Violated(Evidence::ResponseHeadersMissing {
        response_headers_msg: mi,
        response_headers_hop: msg.hop,
        response_headers_ts_us: msg.at_us,
        status: msg.status().unwrap_or_default(),
        missing: owed.iter().map(|s| s.to_string()).collect(),
        branch: msg.via_branch.clone().unwrap_or_default(),
    }))
}

/// **RFC 3261 §8.2.2.3 — a transaction-management request imposes no extension
/// requirement.** `Require` / `Proxy-Require` MUST NOT ride a CANCEL, nor the
/// ACK of a non-2xx final: both are hop-by-hop, both are answered by the
/// transaction layer rather than the core, and a peer that honours the demand
/// would have to 420 a message no 420 can be sent for.
///
/// **A CANCEL is an occasion on its own bytes.** An ACK is one only where this
/// vantage can SHOW it acknowledges a non-2xx: §17.1.1.3 has that ACK generated
/// by the client transaction on the INVITE's own branch, so the branch walk
/// (`super::branch`) pairs it with the final its sender took there. An ACK on
/// a branch carrying no such final is the §13.2.2.4 2xx ACK — a fresh
/// transaction, out of §8.2.2.3's scope — and opens nothing.
///
/// Charges the endpoint that sent the request, which wrote the header.
pub struct NoRequireOnCancelOrAck;

impl Obligation for NoRequireOnCancelOrAck {
    fn id(&self) -> RuleId {
        RuleId::NoRequireOnCancelOrAck
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let branches = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            let acks_non_2xx = || {
                let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                    return false;
                };
                branches
                    .at(&BranchKey::sent_by(msg, branch))
                    .and_then(|b| b.first_final)
                    .and_then(|i| wire.msgs[i].status())
                    .is_some_and(|s| (300..=699).contains(&s))
            };
            if !(method.eq_ignore_ascii_case("CANCEL")
                || (method.eq_ignore_ascii_case("ACK") && acks_non_2xx()))
            {
                continue;
            }
            let finding = |decision| Finding {
                rule: RuleId::NoRequireOnCancelOrAck,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            // Named in §8.2.2.3's own order; a message stating both is ONE
            // offence, reported on the first row it wrote.
            let demanded = ["Require", "Proxy-Require"]
                .into_iter()
                .map(|name| (name, sniff::option_tags(head, name)))
                .find(|(_, tags)| !tags.is_empty());
            let Some((header, tags)) = demanded else {
                out.push(finding(Decision::Compliant));
                continue;
            };
            out.push(finding(Decision::Violated(Evidence::ForbiddenHeaderPresent {
                forbidden_msg: mi,
                forbidden_hop: msg.hop,
                forbidden_ts_us: msg.at_us,
                on: method.clone(),
                header: header.to_string(),
                value: tags.join(", "),
            })));
        }
        out
    }
}

#[cfg(test)]
mod tests;
