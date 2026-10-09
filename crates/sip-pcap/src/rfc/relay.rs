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
mod tests {
    use super::super::testkit::*;
    use super::super::{detect, Census};
    use super::*;

    /// A session description whose origin is `<id> <version>`.
    fn sdp(id: u32, version: u32, port: u16) -> String {
        format!(
            "v=0\r\no=- {id} {version} IN IP4 10.0.0.50\r\ns=-\r\nc=IN IP4 10.0.0.50\r\n\
             t=0 0\r\nm=audio {port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
        )
    }

    /// One call across a B2BUA: the caller A offers on leg-a, the platform P
    /// relays the offer to B on leg-b, B answers and P relays the answer back.
    /// A's ACK carries a description on a completed round, and P's ACK on
    /// leg-b carries `relayed_ack_body`.
    fn ack_body_call(relayed_ack_body: &str) -> FlowsDoc {
        let offer = sdp(100, 1, 4000);
        let answer = sdp(200, 1, 5000);
        let stray = sdp(100, 2, 4000);
        doc_of(vec![
            dg(1_000, A, P, with_body(request("INVITE", 1, "leg-a", "fa", None), &offer)),
            dg(1_100, P, A, response(100, "Trying", 1, "INVITE", "leg-a", "fa", None)),
            dg(
                1_200,
                P,
                B,
                with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer),
            ),
            dg(
                2_000,
                B,
                P,
                with_body(response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb")), &answer),
            ),
            dg(
                2_100,
                P,
                A,
                with_body(response(200, "OK", 1, "INVITE", "leg-a", "fa", Some("tp")), &answer),
            ),
            dg(3_000, A, P, with_body(request("ACK", 1, "leg-a", "fa", Some("tp")), &stray)),
            dg(
                3_100,
                P,
                B,
                with_body(request("ACK", 1, "leg-b", "fp", Some("tb")), relayed_ack_body),
            ),
            dg(9_000_000, A, P, request("BYE", 2, "leg-a", "fa", Some("tp"))),
            dg(9_000_100, P, A, response(200, "OK", 2, "BYE", "leg-a", "fa", Some("tp"))),
            dg(9_000_200, P, B, request("BYE", 2, "leg-b", "fp", Some("tb"))),
            dg(9_000_300, B, P, response(200, "OK", 2, "BYE", "leg-b", "fp", Some("tb"))),
        ])
    }

    fn rule_hits(doc: &FlowsDoc, rule: RfcRule) -> Vec<Hit> {
        detect(doc).into_iter().filter(|h| h.rule == rule).collect()
    }

    /// The platform's ACK carries the caller's own stray description onto the
    /// next leg: the caller's hit is originated, the platform's is relayed
    /// onward and names the caller's as its origin.
    #[test]
    fn a_description_carried_onto_the_next_leg_is_relayed_onward() {
        let doc = ack_body_call(&sdp(100, 2, 4000));
        let hits = rule_hits(&doc, RfcRule::AckBodyAfterCompleteOfferAnswer);
        assert_eq!(hits.len(), 2, "{hits:?}");
        let origin = hits.iter().find(|h| h.emitter == A).expect("the caller's hit");
        let copy = hits.iter().find(|h| h.emitter == P).expect("the platform's hit");
        assert!(!origin.relayed && origin.relays.is_none(), "{origin:?}");
        assert!(copy.relayed, "{copy:?}");
        assert_eq!(
            copy.relays,
            Some(RelayOrigin {
                leg: origin.leg,
                anchor_msg: origin.anchor_msg,
                emitter: A.to_string()
            })
        );
        let json = serde_json::to_value(copy).unwrap();
        assert_eq!(json["relays"]["emitter"], A);
        assert!(serde_json::to_value(origin).unwrap().get("relays").is_none());

        let mut census = Census::new();
        census.absorb("d.json", "cap", &doc);
        let tally = &census.rules["ack-body-after-complete-offer-answer"];
        assert_eq!((tally.hits, tally.relayed), (2, 1));
        assert!(census.summary().contains("originated 1 / relayed onward 1"));
    }

    /// The same session version under rewritten media lines is still the
    /// caller's description carried on.
    #[test]
    fn a_rewritten_description_of_the_same_session_version_is_relayed_onward() {
        let doc = ack_body_call(&sdp(100, 2, 7000));
        let copy = rule_hits(&doc, RfcRule::AckBodyAfterCompleteOfferAnswer)
            .into_iter()
            .find(|h| h.emitter == P)
            .expect("the platform's hit");
        assert!(copy.relayed && copy.relays.is_some(), "{copy:?}");
    }

    /// A description the platform composed itself is its own violation.
    #[test]
    fn a_description_the_platform_composed_is_originated() {
        let doc = ack_body_call(&sdp(300, 1, 6000));
        let copy = rule_hits(&doc, RfcRule::AckBodyAfterCompleteOfferAnswer)
            .into_iter()
            .find(|h| h.emitter == P)
            .expect("the platform's hit");
        assert!(!copy.relayed && copy.relays.is_none(), "{copy:?}");
    }

    /// A 2xx with no answer relayed to the caller forwards the callee's.
    #[test]
    fn an_unanswering_2xx_carried_back_is_relayed_onward() {
        let offer = sdp(100, 1, 4000);
        let doc = doc_of(vec![
            dg(1_000, A, P, with_body(request("INVITE", 1, "leg-a", "fa", None), &offer)),
            dg(
                1_200,
                P,
                B,
                with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer),
            ),
            dg(2_000, B, P, response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb"))),
            dg(2_100, P, A, response(200, "OK", 1, "INVITE", "leg-a", "fa", Some("tp"))),
            dg(2_200, A, P, request("ACK", 1, "leg-a", "fa", Some("tp"))),
            dg(2_300, P, B, request("ACK", 1, "leg-b", "fp", Some("tb"))),
        ]);
        let hits = rule_hits(&doc, RfcRule::Final2xxAnswersTheOffer);
        assert_eq!(hits.len(), 2, "{hits:?}");
        let copy = hits.iter().find(|h| h.emitter == P).expect("the platform's hit");
        assert_eq!(copy.relays.as_ref().map(|o| o.emitter.as_str()), Some(B));
        assert!(!hits.iter().find(|h| h.emitter == B).unwrap().relayed);
    }

    /// A description binding payload type 18 to `encoding`, under origin
    /// `<id> <version>`.
    fn sdp_pt(id: u32, version: u32, encoding: &str) -> String {
        format!(
            "v=0\r\no=- {id} {version} IN IP4 10.0.0.50\r\ns=-\r\nc=IN IP4 10.0.0.50\r\n\
             t=0 0\r\nm=audio 4000 RTP/AVP 18\r\na=rtpmap:18 {encoding}\r\n"
        )
    }

    /// The callee's 200 to the initial INVITE carries no answer and the
    /// platform answers the caller itself; later the platform answers the
    /// caller's re-INVITE with a 200 of its own carrying no answer, never
    /// forwarding that re-INVITE. The two bodiless 200s ride different
    /// transactions: the platform's violation is its own.
    #[test]
    fn probe_bodiless_2xx_of_another_transaction_is_not_a_relay() {
        let offer = sdp(100, 1, 4000);
        let own = sdp(300, 1, 6000);
        let reoffer = sdp(100, 2, 4000);
        let doc = doc_of(vec![
            dg(1_000, A, P, with_body(request("INVITE", 1, "leg-a", "fa", None), &offer)),
            dg(
                1_200,
                P,
                B,
                with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer),
            ),
            dg(2_000, B, P, response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb"))),
            dg(2_050, P, B, request("ACK", 1, "leg-b", "fp", Some("tb"))),
            dg(
                2_100,
                P,
                A,
                with_body(response(200, "OK", 1, "INVITE", "leg-a", "fa", Some("tp")), &own),
            ),
            dg(2_200, A, P, request("ACK", 1, "leg-a", "fa", Some("tp"))),
            dg(5_000, A, P, with_body(request("INVITE", 2, "leg-a", "fa", Some("tp")), &reoffer)),
            dg(5_100, P, A, response(200, "OK", 2, "INVITE", "leg-a", "fa", Some("tp"))),
            dg(5_200, A, P, request("ACK", 2, "leg-a", "fa", Some("tp"))),
        ]);
        let hits = rule_hits(&doc, RfcRule::Final2xxAnswersTheOffer);
        let own = hits.iter().find(|h| h.emitter == P).expect("the platform's own hit");
        assert!(!own.relayed && own.relays.is_none(), "{hits:?}");
    }

    /// The caller re-binds payload type 18; the platform's re-offer keeps the
    /// caller's origin line but binds 18 to yet another encoding — its own
    /// re-binding, not the caller's carried on. The same bytes carried on are.
    #[test]
    fn probe_same_origin_version_different_rebinding_is_not_a_relay() {
        let call = |relayed_reoffer: &str| {
            let offer = sdp_pt(100, 1, "G729/8000");
            let answer = sdp_pt(200, 1, "G729/8000");
            let reoffer = sdp_pt(100, 2, "G729A/8000");
            let reanswer = sdp_pt(200, 2, "G729/8000");
            doc_of(vec![
                dg(1_000, A, P, with_body(request("INVITE", 1, "leg-a", "fa", None), &offer)),
                dg(
                    1_200,
                    P,
                    B,
                    with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer),
                ),
                dg(
                    2_000,
                    B,
                    P,
                    with_body(response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb")), &answer),
                ),
                dg(2_050, P, B, request("ACK", 1, "leg-b", "fp", Some("tb"))),
                dg(
                    2_100,
                    P,
                    A,
                    with_body(response(200, "OK", 1, "INVITE", "leg-a", "fa", Some("tp")), &answer),
                ),
                dg(2_200, A, P, request("ACK", 1, "leg-a", "fa", Some("tp"))),
                dg(
                    5_000,
                    A,
                    P,
                    with_body(request("INVITE", 2, "leg-a", "fa", Some("tp")), &reoffer),
                ),
                dg(
                    5_100,
                    P,
                    B,
                    with_body(request("INVITE", 2, "leg-b", "fp", Some("tb")), relayed_reoffer),
                ),
                dg(
                    5_200,
                    B,
                    P,
                    with_body(
                        response(200, "OK", 2, "INVITE", "leg-b", "fp", Some("tb")),
                        &reanswer,
                    ),
                ),
                dg(5_250, P, B, request("ACK", 2, "leg-b", "fp", Some("tb"))),
                dg(
                    5_300,
                    P,
                    A,
                    with_body(
                        response(200, "OK", 2, "INVITE", "leg-a", "fa", Some("tp")),
                        &reanswer,
                    ),
                ),
                dg(5_400, A, P, request("ACK", 2, "leg-a", "fa", Some("tp"))),
            ])
        };
        let platform = |doc: &FlowsDoc| {
            rule_hits(doc, RfcRule::PayloadTypeMappingStable)
                .into_iter()
                .find(|h| h.emitter == P)
                .expect("the platform's re-binding")
        };
        let rewritten = platform(&call(&sdp_pt(100, 2, "PCMU/8000")));
        assert!(!rewritten.relayed && rewritten.relays.is_none(), "{rewritten:?}");
        let carried = platform(&call(&sdp_pt(100, 2, "G729A/8000")));
        assert_eq!(carried.relays.as_ref().map(|o| o.emitter.as_str()), Some(A), "{carried:?}");
    }

    /// Two forks behind the callee's address answer the platform's INVITE with
    /// a 200 carrying no answer; the platform passes the first on to the caller
    /// and releases the second. The origin is the fork whose 200 won (leg-b,
    /// msg 1), not the latest one to arrive.
    #[test]
    fn the_origin_of_a_relayed_fork_answer_is_the_winning_fork() {
        let offer = sdp(100, 1, 4000);
        let doc = doc_of(vec![
            dg(1_000, A, P, with_body(request("INVITE", 1, "leg-a", "fa", None), &offer)),
            dg(
                1_200,
                P,
                B,
                with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer),
            ),
            dg(2_000, B, P, response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb1"))),
            dg(2_050, B, P, response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb2"))),
            dg(2_100, P, A, response(200, "OK", 1, "INVITE", "leg-a", "fa", Some("tp"))),
            dg(2_200, A, P, request("ACK", 1, "leg-a", "fa", Some("tp"))),
            dg(2_300, P, B, request("ACK", 1, "leg-b", "fp", Some("tb1"))),
            dg(2_310, P, B, request("ACK", 1, "leg-b", "fp", Some("tb2"))),
            dg(2_320, P, B, request("BYE", 2, "leg-b", "fp", Some("tb2"))),
            dg(2_400, B, P, response(200, "OK", 2, "BYE", "leg-b", "fp", Some("tb2"))),
        ]);
        let hits = rule_hits(&doc, RfcRule::Final2xxAnswersTheOffer);
        assert_eq!(hits.len(), 3, "both forks and the platform: {hits:?}");
        let copy = hits.iter().find(|h| h.emitter == P).expect("the platform's hit");
        assert_eq!(copy.relays.as_ref().map(|o| (o.emitter.as_str(), o.anchor_msg)), Some((B, 1)));
    }
}
