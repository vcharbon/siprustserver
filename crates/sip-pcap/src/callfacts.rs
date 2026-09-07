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
/// timers. Measured across the corpus each of them sits on T1: a median gap of
/// 500 ms, which is the first rung and nothing else.
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
/// ingest, before any relation is stated. Issue 116 measured the classes.
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
            (0..i)
                .find(|&j| repeats(&leg.msgs[i], &leg.msgs[j]))
                .filter(|&j| leg.msgs[i].ts_us.saturating_sub(leg.msgs[j].ts_us) <= REPEAT_ENVELOPE_US)
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
            legs[l].msgs.iter().enumerate().map(move |(i, m)| Coord { leg: l, msg: i, ts_us: m.ts_us })
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
mod tests {
    use super::*;
    use crate::doc::{CSeqJson, Party, Payload, ViaJson};

    /// The datagram a message's projections describe, rendered so the byte
    /// bound has real bytes to read: every field the criteria key on is IN the
    /// payload, exactly as it would be on the wire.
    fn wire(summary: &Summary, branch: &str) -> String {
        let (start, cseq) = match summary {
            Summary::Request { method, uri, cseq, .. } => {
                (format!("{method} {uri} SIP/2.0"), cseq)
            }
            Summary::Response { status, reason, cseq, .. } => {
                (format!("SIP/2.0 {status} {reason}"), cseq)
            }
        };
        format!(
            "{start}\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\nCSeq: {} {}\r\n\r\n",
            cseq.seq, cseq.method
        )
    }

    fn msg_of(ts_us: u64, src: &str, dst: &str, summary: Summary, branch: &str) -> MsgJson {
        MsgJson {
            ts_us,
            src: src.into(),
            dst: dst.into(),
            hop: 0,
            retx: false,
            probe: 0,
            repeat_of: None,
            payload: Payload::Text { raw: wire(&summary, branch) },
            summary,
            via: vec![ViaJson {
                sent_by: "10.0.0.1:5060".into(),
                transport: "UDP".into(),
                branch: Some(branch.into()),
                received: None,
            }],
            headers: Vec::new(),
            identities: crate::doc::Identities::default(),
            rseq: None,
            replaces: None,
            refer_to: None,
            body: None,
        }
    }

    /// The same message carrying a body, appended past the blank line that
    /// already ends its head.
    fn with_body(mut m: MsgJson, body: &str) -> MsgJson {
        let Payload::Text { raw } = &m.payload else { unreachable!("the helper renders text") };
        m.payload = Payload::Text { raw: format!("{raw}{body}") };
        m
    }

    /// The same answer from a different early dialog: a To-tag, in the
    /// projection AND on the wire.
    fn with_to_tag(mut m: MsgJson, tag: &str) -> MsgJson {
        let Payload::Text { raw } = &m.payload else { unreachable!("the helper renders text") };
        m.payload = Payload::Text {
            raw: raw.replace("\r\n\r\n", &format!("\r\nTo: <sip:a@h>;tag={tag}\r\n\r\n")),
        };
        if let Summary::Response { to, .. } = &mut m.summary {
            to.tag = Some(tag.into());
        }
        m
    }

    /// The same message stating its RSeq, in the projection AND on the wire.
    fn with_rseq(mut m: MsgJson, v: &str) -> MsgJson {
        let Payload::Text { raw } = &m.payload else { unreachable!("the helper renders text") };
        m.payload = Payload::Text { raw: raw.replace("\r\n\r\n", &format!("\r\nRSeq: {v}\r\n\r\n")) };
        m.rseq = Some(v.into());
        m
    }

    fn party() -> Party {
        Party { uri: "sip:a@h".into(), tag: None }
    }

    fn request(method: &str, seq: u32) -> Summary {
        Summary::Request {
            method: method.into(),
            uri: "sip:b@h".into(),
            cseq: CSeqJson { seq, method: method.into() },
            from: party(),
            to: party(),
        }
    }

    fn response(status: u16, seq: u32) -> Summary {
        Summary::Response {
            status,
            reason: "x".into(),
            cseq: CSeqJson { seq, method: "INVITE".into() },
            from: party(),
            to: party(),
        }
    }

    fn leg(msgs: Vec<MsgJson>) -> LegJson {
        LegJson {
            call_id: "c1".into(),
            hops: Vec::new(),
            invite: None,
            final_status: None,
            saw_180: false,
            terminated_by: None,
            tokens: Vec::new(),
            msgs,
        }
    }

    /// A peer that re-ACKs each retransmitted final mints a fresh branch every
    /// time, so its ACKs are not the same datagram and none of them repeats.
    /// Ruling Q34 collapsed them onto one count to stop a ladder being encoded
    /// twice; the byte bound withdraws that, because a count replays copies of
    /// ONE stored ACK and these differ on the wire. The platform's own 200s,
    /// re-sent unchanged, still repeat.
    #[test]
    fn a_fresh_branch_re_ack_is_not_the_ack_before_it() {
        let mut l = leg(vec![
            msg_of(1_000, "a", "b", response(200, 1), "z9hG4bKf1"),
            msg_of(2_000, "b", "a", request("ACK", 1), "z9hG4bKa1"),
            msg_of(3_000, "a", "b", response(200, 1), "z9hG4bKf1"),
            msg_of(4_000, "b", "a", request("ACK", 1), "z9hG4bKa2"),
            msg_of(5_000, "b", "a", request("ACK", 1), "z9hG4bKa3"),
        ]);
        mark_repeats(&mut l);
        let repeats: Vec<Option<usize>> = l.msgs.iter().map(|m| m.repeat_of).collect();
        assert_eq!(repeats, vec![None, None, Some(0), None, None]);
    }

    /// RFC 3262 §3: one INVITE transaction carries several DISTINCT reliable
    /// provisionals told apart only by RSeq — same RSeq is a retransmission,
    /// a different RSeq is a new message.
    #[test]
    fn a_reliable_provisional_repeats_only_on_the_same_rseq() {
        let mut l = leg(vec![
            with_rseq(msg_of(1_000, "a", "b", response(183, 1), "z9hG4bK1"), "366986"),
            with_rseq(msg_of(2_000, "a", "b", response(183, 1), "z9hG4bK1"), "366986"),
            with_rseq(msg_of(3_000, "a", "b", response(183, 1), "z9hG4bK1"), "366987"),
        ]);
        mark_repeats(&mut l);
        let repeats: Vec<Option<usize>> = l.msgs.iter().map(|m| m.repeat_of).collect();
        assert_eq!(repeats, vec![None, Some(0), None]);
    }

    /// An unreliable provisional rides no timer, so it does not retransmit: a
    /// platform that rings twice has SENT TWICE however alike the bytes, and
    /// both copies are events. The one thing that duplicates such a datagram
    /// without the platform emitting it is the capture stack, and the ingest
    /// dedup window takes that before any relation is stated.
    #[test]
    fn an_unreliable_provisional_never_repeats() {
        let mut l = leg(vec![
            msg_of(1_000_000, "a", "b", response(180, 1), "z9hG4bK1"),
            msg_of(1_002_000, "a", "b", response(180, 1), "z9hG4bK1"),
            msg_of(1_402_000, "a", "b", response(180, 1), "z9hG4bK1"),
        ]);
        mark_repeats(&mut l);
        assert_eq!(
            l.msgs.iter().map(|m| (m.repeat_of, m.retx)).collect::<Vec<_>>(),
            vec![(None, false), (None, false), (None, false)],
            "two milliseconds apart or four hundred, each ring is its own emission"
        );
    }

    /// The two classes that DO ride a ladder keep the relation, so bounding the
    /// unreliable one withdraws nothing else: a reliable provisional is paced by
    /// RFC 3262 §3, and a 100 Trying's copies are DRAWN by the INVITE ladder
    /// that draws them (RFC 3261 §17.2.1).
    #[test]
    fn the_classes_that_ride_a_ladder_still_repeat() {
        let mut reliable = leg(vec![
            with_rseq(msg_of(1_000_000, "a", "b", response(180, 1), "z9hG4bK1"), "1"),
            with_rseq(msg_of(1_500_000, "a", "b", response(180, 1), "z9hG4bK1"), "1"),
        ]);
        mark_repeats(&mut reliable);
        assert_eq!(reliable.msgs[1].repeat_of, Some(0), "RFC 3262 §3 paces this one");

        let mut trying = leg(vec![
            msg_of(1_000_000, "a", "b", response(100, 1), "z9hG4bK1"),
            msg_of(1_500_000, "a", "b", response(100, 1), "z9hG4bK1"),
        ]);
        mark_repeats(&mut trying);
        assert_eq!(trying.msgs[1].repeat_of, Some(0), "one 100 per copy of the INVITE");
    }

    /// A ladder inside 64·T1 is one transaction re-emitting; the copy that
    /// lands past the envelope is a FRESH emission, and so is every later one —
    /// the anchor is the EARLIEST match, so a re-emission never becomes the
    /// head of a new ladder on the strength of a near neighbour.
    #[test]
    fn a_repeat_past_the_transaction_envelope_is_a_fresh_emission() {
        let mut l = leg(vec![
            msg_of(1_000_000, "a", "b", response(200, 1), "z9hG4bK1"),
            msg_of(31_000_000, "a", "b", response(200, 1), "z9hG4bK1"),
            msg_of(61_000_000, "a", "b", response(200, 1), "z9hG4bK1"),
            msg_of(61_500_000, "a", "b", response(200, 1), "z9hG4bK1"),
        ]);
        mark_repeats(&mut l);
        let repeats: Vec<Option<usize>> = l.msgs.iter().map(|m| m.repeat_of).collect();
        assert_eq!(repeats, vec![None, Some(0), None, None]);
        let retx: Vec<bool> = l.msgs.iter().map(|m| m.retx).collect();
        assert_eq!(retx, vec![false, true, false, false]);
    }

    /// A message that grew a body is not the one before it re-sent: §6.9
    /// collapses a repeat onto a `retransmits` count, and a count replays N
    /// copies of ONE stored message, so a 183 that gained an SDP offer must
    /// stay a step of its own. The bodiless pair on either side still repeats.
    #[test]
    fn a_repeat_carries_the_same_body_as_what_it_repeats() {
        let bare = || with_rseq(msg_of(0, "a", "b", response(183, 1), "z9hG4bK1"), "1");
        let mut l = leg(vec![
            MsgJson { ts_us: 1_000_000, ..bare() },
            MsgJson { ts_us: 2_000_000, ..bare() },
            with_body(MsgJson { ts_us: 3_000_000, ..bare() }, "v=0 offer"),
            with_body(MsgJson { ts_us: 4_000_000, ..bare() }, "v=0 offer"),
            with_body(MsgJson { ts_us: 5_000_000, ..bare() }, "v=0 answer"),
        ]);
        mark_repeats(&mut l);
        assert_eq!(
            l.msgs.iter().map(|m| (m.repeat_of, m.retx)).collect::<Vec<_>>(),
            vec![(None, false), (Some(0), true), (None, false), (Some(2), true), (None, false)]
        );
    }

    /// Two forked callees answering through one proxy hop share the top Via
    /// branch, the status and the CSeq, so transaction identity alone calls
    /// them one message — and the count then replays one fork's answer twice
    /// while the other early dialog leaves the trace. Their To-tags differ on
    /// the wire, so the byte bound withdraws the relation without the criteria
    /// having to learn what a dialog is.
    #[test]
    fn two_forks_answering_under_one_branch_are_two_messages() {
        let ring = |ts_us, tag: &str| {
            with_to_tag(with_rseq(msg_of(ts_us, "a", "b", response(183, 1), "z9hG4bK1"), "1"), tag)
        };
        let mut l = leg(vec![ring(1_000, "fork-a"), ring(2_000, "fork-b"), ring(3_000, "fork-a")]);
        mark_repeats(&mut l);
        assert_eq!(
            l.msgs.iter().map(|m| m.repeat_of).collect::<Vec<_>>(),
            vec![None, None, Some(0)],
            "each fork repeats only its own answer"
        );
    }

    /// An opaque payload is stored whole, so it is compared whole and never
    /// against a readable one — two unparseable datagrams repeat only when
    /// they are the same bytes.
    #[test]
    fn an_opaque_payload_repeats_only_itself() {
        let bare = || msg_of(0, "b", "a", request("BYE", 2), "z9hG4bK1");
        let opaque = |ts_us, raw_b64: &str| MsgJson {
            ts_us,
            payload: Payload::Opaque { raw_b64: raw_b64.into() },
            ..bare()
        };
        let mut l = leg(vec![
            opaque(1_000_000, "AAEC"),
            opaque(2_000_000, "AAEC"),
            opaque(3_000_000, "AAED"),
            MsgJson { ts_us: 4_000_000, ..bare() },
        ]);
        mark_repeats(&mut l);
        assert_eq!(
            l.msgs.iter().map(|m| m.repeat_of).collect::<Vec<_>>(),
            vec![None, Some(0), None, None]
        );
    }

    /// `retx` is the same-branch half of the relation, and the byte bound makes
    /// the two halves coincide: identical bytes carry an identical branch, so a
    /// relation can no longer hold where the branch was re-minted.
    #[test]
    fn retx_and_the_relation_now_name_the_same_messages() {
        let mut l = leg(vec![
            msg_of(1_000, "b", "a", request("ACK", 1), "z9hG4bKa1"),
            msg_of(2_000, "b", "a", request("ACK", 1), "z9hG4bKa1"),
            msg_of(3_000, "b", "a", request("ACK", 1), "z9hG4bKa2"),
        ]);
        mark_repeats(&mut l);
        assert_eq!(
            l.msgs.iter().map(|m| (m.repeat_of, m.retx)).collect::<Vec<_>>(),
            vec![(None, false), (Some(0), true), (None, false)]
        );
    }

    /// A response with a different branch is another transaction's answer, not
    /// a repeat, and the opposite direction is never a repeat either.
    #[test]
    fn a_different_branch_or_direction_is_not_a_repeat() {
        let mut l = leg(vec![
            msg_of(1_000, "a", "b", response(200, 1), "z9hG4bK1"),
            msg_of(2_000, "a", "b", response(200, 1), "z9hG4bK2"),
            msg_of(3_000, "b", "a", response(200, 1), "z9hG4bK1"),
        ]);
        mark_repeats(&mut l);
        assert!(l.msgs.iter().all(|m| m.repeat_of.is_none()));
    }

    /// The call's terminal status is the LAST INVITE final in the group, not
    /// the first leg's; the initial INVITE is the earliest one anywhere.
    #[test]
    fn the_call_summary_reads_across_every_leg() {
        let legs = vec![
            leg(vec![
                msg_of(1_000, "a", "b", request("INVITE", 1), "z1"),
                msg_of(9_000, "b", "a", response(486, 1), "z1"),
            ]),
            leg(vec![
                msg_of(2_000, "b", "c", request("INVITE", 1), "z2"),
                msg_of(8_000, "c", "b", response(200, 1), "z2"),
            ]),
        ];
        let mut group = GroupJson {
            legs: vec![0, 1],
            evidence: Vec::new(),
            t0_us: 0,
            initial_invite: None,
            final_us: None,
            final_status: None,
            methods: BTreeMap::new(),
        };
        summarize(&legs, &mut group);
        assert_eq!(group.t0_us, 1_000);
        let initial = group.initial_invite.expect("the call opens with an INVITE");
        assert_eq!((initial.leg, initial.msg), (0, 0));
        assert_eq!(group.final_status, Some(486));
        assert_eq!(group.final_us, Some(9_000));
        assert_eq!(group.methods["INVITE"].requests, 2);
    }
}
