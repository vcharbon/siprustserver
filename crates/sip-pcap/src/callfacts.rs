//! Facts no single message carries: which message repeats which along a leg,
//! and the per-call summary a consumer would otherwise recompute by scanning
//! every message of every leg.
//!
//! Both read the emitted document rather than the wire bytes — they are
//! relations between messages, and the projections they need
//! ([`crate::msgfacts`]) are already on the page.

use std::collections::{BTreeMap, BTreeSet};

use crate::doc::{GroupJson, LegJson, MethodFacts, MsgJson, MsgRef, Payload, Summary};

/// The transaction envelope a repeat must fall inside: 64·T1 = 32 s.
///
/// Every RETRANSMITTING class shares that ceiling — Timer B (RFC 3261
/// §17.1.1.2), Timer F (§17.1.2.2), Timer H (§17.2.1) and a reliable
/// provisional's own ladder (RFC 3262 §3), which reuses the final response's
/// timers. Each of them opens on T1 (500 ms by default), the first rung and
/// nothing else.
///
/// The unreliable provisional is not one of them — see
/// [`retransmits_on_a_timer`].
pub const REPEAT_ENVELOPE_US: u64 = 32_000_000;

/// Whether this message belongs to a class that RETRANSMITS, so that a later
/// copy of it can be a repeat rather than a fresh emission.
///
/// An unreliable provisional — a 1xx above 100 carrying no `RSeq` — does not:
/// it rides no timer, RFC 3262 §3 pacing only the RELIABLE one, so a platform
/// that rings twice has SENT TWICE and both copies are events. A 100 Trying
/// keeps the relation, its copies DRAWN one per copy of the INVITE (§17.2.1).
/// The capture stack is the one thing that duplicates a datagram the platform
/// sent once, and [`crate::flow::FlowConfig::dedup_window_us`] takes that at
/// ingest, before any relation is stated.
fn retransmits_on_a_timer(m: &MsgJson) -> bool {
    match &m.summary {
        Summary::Response { status, .. } if *status > 100 && *status < 200 => m.rseq.is_some(),
        _ => true,
    }
}

/// Mark each message with the earliest message it repeats, and tag the
/// same-transaction half of that relation as `retx`.
///
/// **A repeat is [`identical`] to what it repeats, byte for byte.** That is a
/// necessary condition and never a sufficient one: it can only ever WITHDRAW a
/// relation the criteria below would otherwise state. §6.9 collapses a repeat
/// onto a `retransmits` count, and a count replays N copies of the ONE stored
/// message — so anything a peer could read differently makes two messages, not
/// one. A provisional that grew an SDP offer, a second fork's answer under one
/// Via branch, one that gained `P-Early-Media` are each two. The status never
/// enters it: 180 and 183 answer to the same rule, and so does every other.
///
/// Then two criteria, one per message kind, both same-leg, same-direction:
///
/// - a REQUEST repeats an earlier request of the same method and CSeq number.
/// - a RESPONSE repeats an earlier response with the same status, CSeq, Via
///   branch AND RSeq — one transaction re-emitting its answer. RFC 3262 §3
///   sequences several DISTINCT reliable provisionals through one INVITE
///   transaction, so two same-status 18x that differ in RSeq are two messages.
///
/// **A repeat is bounded by [`REPEAT_ENVELOPE_US`], measured from the earliest
/// emission.** Past it the transaction is over and nothing is retransmitting,
/// so bytes that match are a FRESH event — a ringing refresh (RFC 3261
/// §13.3.1.1 obliges a non-100 provisional every minute), a re-offer — and
/// every consumer that reads a repeat as "not an event" must see it.
///
/// **And a class that rides no timer states no relation at all**
/// ([`retransmits_on_a_timer`]): an unreliable provisional does not retransmit,
/// so every emission of one is its own event however alike the bytes.
///
/// `retx` is the same-transaction half: the relation holds AND the top Via
/// branch matches. Identical bytes carry an identical branch, so the two halves
/// now coincide and the field says what it always meant. It is decided here,
/// never at ingest — the two must not be able to disagree, because a consumer
/// reading `retx` where `repeat_of` is absent falls back to a criterion this
/// one replaced.
pub fn mark_repeats(leg: &mut LegJson) {
    for i in 0..leg.msgs.len() {
        let anchor = if retransmits_on_a_timer(&leg.msgs[i]) {
            (0..i).find(|&j| repeats(&leg.msgs[i], &leg.msgs[j])).filter(|&j| {
                leg.msgs[i].ts_us.saturating_sub(leg.msgs[j].ts_us) <= REPEAT_ENVELOPE_US
            })
        } else {
            None
        };
        let retx = anchor.is_some_and(|j| branch(&leg.msgs[i]) == branch(&leg.msgs[j]));
        leg.msgs[i].repeat_of = anchor;
        leg.msgs[i].retx = retx;
    }
}

fn repeats(later: &MsgJson, earlier: &MsgJson) -> bool {
    if later.src != earlier.src || later.dst != earlier.dst {
        return false;
    }
    if !identical(later, earlier) {
        return false;
    }
    match (&later.summary, &earlier.summary) {
        (
            Summary::Request { method: m1, cseq: c1, .. },
            Summary::Request { method: m2, cseq: c2, .. },
        ) => m1 == m2 && c1.seq == c2.seq,
        (
            Summary::Response { status: s1, cseq: c1, .. },
            Summary::Response { status: s2, cseq: c2, .. },
        ) => {
            s1 == s2
                && c1.seq == c2.seq
                && c1.method == c2.method
                && branch(later) == branch(earlier)
                && rseq(later) == rseq(earlier)
        }
        _ => false,
    }
}

/// Whether two messages are the SAME DATAGRAM, byte for byte.
///
/// [`Payload::of`] chooses the form from the bytes alone, so identical bytes
/// take identical forms and the variants compare directly — no decode, and a
/// readable payload never equals an opaque one.
fn identical(later: &MsgJson, earlier: &MsgJson) -> bool {
    match (&later.payload, &earlier.payload) {
        (Payload::Text { raw: a }, Payload::Text { raw: b }) => a == b,
        (
            Payload::HeadBody { head: h1, body_b64: b1 },
            Payload::HeadBody { head: h2, body_b64: b2 },
        ) => h1 == h2 && b1 == b2,
        (Payload::Opaque { raw_b64: a }, Payload::Opaque { raw_b64: b }) => a == b,
        _ => false,
    }
}

fn branch(m: &MsgJson) -> Option<&str> {
    m.via.first()?.branch.as_deref()
}

fn rseq(m: &MsgJson) -> Option<&str> {
    m.rseq.as_deref()
}

/// Compute a group's per-call summary from its member legs.
pub fn summarize(legs: &[LegJson], group: &mut GroupJson) {
    let ordered = capture_order(legs, group);
    group.t0_us = ordered.first().map(|c| c.ts_us).unwrap_or(0);
    group.initial_invite = ordered
        .iter()
        .find(|c| matches!(&msg(legs, c).summary, Summary::Request { method, .. } if method == "INVITE"))
        .map(|c| MsgRef { leg: c.leg, msg: c.msg });
    // The call's terminal status: the LAST response with status >= 200 to an
    // INVITE, wherever in the group it landed. A group whose attempts live on
    // several legs exposes several per-leg finals; this is the one that ended
    // the call.
    let terminal = ordered.iter().rev().find_map(|c| match &msg(legs, c).summary {
        Summary::Response { status, cseq, .. } if *status >= 200 && cseq.method == "INVITE" => {
            Some((*status, c.ts_us))
        }
        _ => None,
    });
    group.final_status = terminal.map(|(status, _)| status);
    group.final_us = terminal.map(|(_, ts)| ts);
    group.methods = method_facts(legs, &ordered);
}

/// A message coordinate inside the document, resolved to the emitted leg id.
struct Coord {
    leg: usize,
    msg: usize,
    ts_us: u64,
}

fn msg<'a>(legs: &'a [LegJson], c: &Coord) -> &'a MsgJson {
    &legs[c.leg].msgs[c.msg]
}

/// Every message of the group in capture-time order, ties broken by member
/// order then by position within the leg.
fn capture_order(legs: &[LegJson], group: &GroupJson) -> Vec<Coord> {
    let mut out: Vec<Coord> = group
        .legs
        .iter()
        .filter(|&&l| l < legs.len())
        .flat_map(|&l| {
            legs[l].msgs.iter().enumerate().map(move |(i, m)| Coord {
                leg: l,
                msg: i,
                ts_us: m.ts_us,
            })
        })
        .collect();
    out.sort_by_key(|c| c.ts_us);
    out
}

fn method_facts(legs: &[LegJson], ordered: &[Coord]) -> BTreeMap<String, MethodFacts> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut types: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for c in ordered {
        let m = msg(legs, c);
        let Summary::Request { method, .. } = &m.summary else { continue };
        *counts.entry(method.clone()).or_default() += 1;
        let bucket = types.entry(method.clone()).or_default();
        if let Some(body) = &m.body {
            if !body.content_type.is_empty() {
                bucket.insert(body.content_type.clone());
            }
            for part in &body.parts {
                let head = part.content_type.split(';').next().unwrap_or("").trim();
                if !head.is_empty() {
                    bucket.insert(head.to_string());
                }
            }
        }
    }
    counts
        .into_iter()
        .map(|(method, requests)| {
            let content_types =
                types.remove(&method).map(|t| t.into_iter().collect()).unwrap_or_default();
            (method, MethodFacts { requests, content_types })
        })
        .collect()
}

#[cfg(test)]
mod tests;
