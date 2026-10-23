//! Relayed onward: a violation one endpoint of a call commits and the box the
//! call's legs cross forwards, read across the legs of one call group.
//!
//! The rules decide one leg at a time, so the forwarding box's copy of a
//! violation on the next leg reads as a second, originated hit. This pass
//! pairs that copy with the hit it forwards: a hit of a [`RELAYABLE`] rule is
//! RELAYED ONWARD where a hit of the same rule, on another leg of the same
//! group, was emitted toward the host that emits this one INSIDE THE COPY'S
//! TRANSACTION, and the two anchor messages are one message carried on: the
//! same kind, the same session description — byte for byte, or by its origin's
//! session id and version with the same violation judged in it — or absent
//! from both. The hit keeps its charge and names its origin.
//!
//! **Inside the transaction.** A response copy answers the request its host
//! took on the copy leg; its origin is a response to a request that host sent
//! on the origin leg after taking that one. An ACK copy acknowledges the 2xx
//! its host took; its origin is an ACK the host took after that 2xx. A request
//! copy's origin arrived after the host's previous request of that method on
//! the copy leg. Of several candidates the EARLIEST is the origin: the first
//! answer is the one a forwarding box passes on.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::doc::{FlowsDoc, MsgJson, Summary};

use super::{endpoint_ip, Evidence, Hit, RfcRule};

/// The rules a copy can relay onward ([`RfcRule::RELAYABLE`]).
pub const RELAYABLE: &[RfcRule] = RfcRule::RELAYABLE;

/// The originated hit a relayed one forwards, located in the same document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOrigin {
    /// Index into `doc.legs` of the origin's leg.
    pub leg: usize,
    /// Index into that leg's `msgs` of the origin's anchor message.
    pub anchor_msg: usize,
    /// The endpoint the origin charges.
    pub emitter: String,
}

/// Mark every hit of `hits` — one call group's — that relays another of them
/// onward: `relayed` set, `relays` naming the origin. An origin is paired with
/// one relayed copy at most.
pub(super) fn link(doc: &FlowsDoc, hits: &mut [Hit]) {
    let mut paired: BTreeSet<usize> = BTreeSet::new();
    let mut order: Vec<usize> = (0..hits.len()).collect();
    order.sort_by_key(|&i| at_us(doc, &hits[i]));
    for &copy in &order {
        if !RELAYABLE.contains(&hits[copy].rule) || hits[copy].relays.is_some() {
            continue;
        }
        let Some(window) = Window::of(doc, &hits[copy]) else { continue };
        let origin = order
            .iter()
            .copied()
            .filter(|&o| o != copy && !paired.contains(&o) && hits[o].relays.is_none())
            .find(|&o| forwards(doc, &hits[o], &hits[copy], &window));
        let Some(origin) = origin else { continue };
        paired.insert(origin);
        let relays = RelayOrigin {
            leg: hits[origin].leg,
            anchor_msg: hits[origin].anchor_msg,
            emitter: hits[origin].emitter.clone(),
        };
        hits[copy].relayed = true;
        hits[copy].relays = Some(relays);
    }
}

/// Where a copy's origin may sit: the copy's own message and host, and the
/// instant its transaction opened at the forwarding host.
struct Window<'d> {
    copy: &'d MsgJson,
    /// When the host took what opened the copy's exchange: the request a
    /// response copy answers, the 2xx an ACK copy acknowledges, or its own
    /// previous request of the method; zero where it sent none before.
    opened_us: u64,
}

impl<'d> Window<'d> {
    /// The window of `hit`'s anchor; `None` where its leg does not carry what
    /// opened its transaction, so nothing can be placed inside it.
    fn of(doc: &'d FlowsDoc, hit: &Hit) -> Option<Window<'d>> {
        let msgs = &doc.legs.get(hit.leg)?.msgs;
        let copy = msgs.get(hit.anchor_msg)?;
        let before = msgs[..hit.anchor_msg].iter().rev();
        let opened_us = match &copy.summary {
            Summary::Response { cseq, .. } => {
                before
                    .filter(|m| same_host(&m.dst, &copy.src))
                    .find(|m| is_request(m, &cseq.method, Some(cseq.seq)))?
                    .ts_us
            }
            Summary::Request { method, cseq, .. } if method.eq_ignore_ascii_case("ACK") => {
                before
                    .filter(|m| same_host(&m.dst, &copy.src))
                    .find(|m| is_2xx_to_invite(m, cseq.seq))?
                    .ts_us
            }
            Summary::Request { method, .. } => before
                .filter(|m| same_host(&m.src, &copy.src))
                .find(|m| is_request(m, method, None))
                .map_or(0, |m| m.ts_us),
        };
        Some(Window { copy, opened_us })
    }
}

/// Whether `copy` forwards `origin`: same rule, on another leg, a different
/// emitter, taken by the copy's emitting host inside the copy's transaction
/// and before the copy went out, and one message carried on.
fn forwards(doc: &FlowsDoc, origin: &Hit, copy: &Hit, window: &Window<'_>) -> bool {
    if origin.rule != copy.rule || origin.leg == copy.leg || origin.emitter == copy.emitter {
        return false;
    }
    let Some(origin_msg) = anchor(doc, origin) else { return false };
    same_host(&origin_msg.dst, &window.copy.src)
        && window.opened_us <= origin_msg.ts_us
        && origin_msg.ts_us <= window.copy.ts_us
        && answers_a_forwarded_request(doc, origin, origin_msg, window)
        && same_kind(origin_msg, window.copy)
        && same_description(origin_msg, window.copy, &origin.evidence, &copy.evidence)
}

/// A response origin answers a request the copy's host sent on the origin leg
/// once its own transaction had opened; any other origin passes.
fn answers_a_forwarded_request(
    doc: &FlowsDoc,
    origin: &Hit,
    origin_msg: &MsgJson,
    window: &Window<'_>,
) -> bool {
    let Summary::Response { cseq, .. } = &origin_msg.summary else { return true };
    let Some(leg) = doc.legs.get(origin.leg) else { return false };
    leg.msgs[..origin.anchor_msg]
        .iter()
        .rev()
        .filter(|m| same_host(&m.src, &window.copy.src))
        .find(|m| is_request(m, &cseq.method, Some(cseq.seq)))
        .is_some_and(|m| window.opened_us <= m.ts_us)
}

/// Whether two endpoints sit on one host: the same IP, or the same token
/// where either is no socket address.
fn same_host(a: &str, b: &str) -> bool {
    match (endpoint_ip(a), endpoint_ip(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// A request of `method`, under CSeq number `seq` where one is given.
fn is_request(m: &MsgJson, method: &str, seq: Option<u32>) -> bool {
    matches!(&m.summary, Summary::Request { method: x, cseq, .. }
        if x.eq_ignore_ascii_case(method) && seq.is_none_or(|n| cseq.seq == n))
}

/// A 2xx to the INVITE numbered `seq`.
fn is_2xx_to_invite(m: &MsgJson, seq: u32) -> bool {
    matches!(&m.summary, Summary::Response { status, cseq, .. }
        if (200..300).contains(status) && cseq.seq == seq && cseq.method.eq_ignore_ascii_case("INVITE"))
}

/// The message a hit rests on.
fn anchor<'d>(doc: &'d FlowsDoc, hit: &Hit) -> Option<&'d MsgJson> {
    doc.legs.get(hit.leg)?.msgs.get(hit.anchor_msg)
}

/// When a hit's anchor message was captured; the end of time where the
/// document does not carry it, so such a hit sorts last and pairs with nothing.
fn at_us(doc: &FlowsDoc, hit: &Hit) -> u64 {
    anchor(doc, hit).map_or(u64::MAX, |m| m.ts_us)
}

/// The same request method, or the same status to the same method: CSeq
/// numbers differ across legs and say nothing here.
fn same_kind(a: &MsgJson, b: &MsgJson) -> bool {
    match (&a.summary, &b.summary) {
        (Summary::Request { method: x, .. }, Summary::Request { method: y, .. }) => {
            x.eq_ignore_ascii_case(y)
        }
        (
            Summary::Response { status: x, cseq: cx, .. },
            Summary::Response { status: y, cseq: cy, .. },
        ) => x == y && cx.method.eq_ignore_ascii_case(&cy.method),
        _ => false,
    }
}

/// Whether the two messages carry one session description: none on either,
/// the same bytes, or the same origin session id and version (RFC 4566 §5.2)
/// under which the rule judged the same violation — a forwarding hop may
/// rewrite the lines around a description, never what makes it the offence.
fn same_description(a: &MsgJson, b: &MsgJson, judged_a: &Evidence, judged_b: &Evidence) -> bool {
    match (body_of(a), body_of(b)) {
        (None, None) => true,
        (Some(x), Some(y)) if x == y => true,
        (Some(x), Some(y)) => {
            let same_version = match (
                sip_message::sdp_doc::parse_origin(&x),
                sip_message::sdp_doc::parse_origin(&y),
            ) {
                (Some(ox), Some(oy)) => {
                    ox.session_id == oy.session_id && ox.session_version == oy.session_version
                }
                _ => false,
            };
            same_version && same_judgement(judged_a, judged_b)
        }
        _ => false,
    }
}

/// Whether the rule found the same thing wrong in both: the re-bound payload
/// types and the encodings they were re-bound to, the stream table an ACK body
/// carried, the status of a final that answered nothing.
fn same_judgement(a: &Evidence, b: &Evidence) -> bool {
    match (a, b) {
        (
            Evidence::PayloadTypeRemapped { payload_types: pa, encodings: ea, .. },
            Evidence::PayloadTypeRemapped { payload_types: pb, encodings: eb, .. },
        ) => pa == pb && ea == eb,
        (
            Evidence::AckBodyOnClosedRound { streams: sa, .. },
            Evidence::AckBodyOnClosedRound { streams: sb, .. },
        ) => sa == sb,
        (
            Evidence::OfferLeftUnanswered { status: sa, .. },
            Evidence::OfferLeftUnanswered { status: sb, .. },
        ) => sa == sb,
        _ => false,
    }
}

/// The message's body, `None` where it carries none.
fn body_of(m: &MsgJson) -> Option<Vec<u8>> {
    m.payload.body().filter(|b| !b.is_empty())
}

#[cfg(test)]
mod tests;
