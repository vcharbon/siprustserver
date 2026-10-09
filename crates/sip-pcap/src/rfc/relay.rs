//! Relayed onward: a violation one endpoint of a call commits and the box the
//! call's legs cross forwards, read across the legs of one call group.
//!
//! The rules decide one leg at a time, so the forwarding box's copy of a
//! violation on the next leg reads as a second, originated hit. This pass
//! pairs that copy with the hit it forwards: a hit of a [`RELAYABLE`] rule is
//! RELAYED ONWARD where an earlier hit of the same rule, on another leg of the
//! same group, was emitted toward the host that emits this one, and the two
//! anchor messages are one message carried on: the same kind, and the same
//! session description — byte for byte, by its origin's session id and
//! version, or absent from both. The hit keeps its charge and names its origin.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::doc::{FlowsDoc, MsgJson, Summary};

use super::{endpoint_ip, Hit, RfcRule};

/// The rules whose offence the anchor message carries in its own content, so
/// a hop that forwards the message unchanged forwards the violation with it.
pub const RELAYABLE: [RfcRule; 3] = [
    RfcRule::PayloadTypeMappingStable,
    RfcRule::Final2xxAnswersTheOffer,
    RfcRule::AckBodyAfterCompleteOfferAnswer,
];

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
/// one relayed copy at most, the latest unpaired origin before the copy, so a
/// later exchange on the call never claims an earlier one's violation.
pub(super) fn link(doc: &FlowsDoc, hits: &mut [Hit]) {
    let mut paired: BTreeSet<usize> = BTreeSet::new();
    let mut order: Vec<usize> = (0..hits.len()).collect();
    order.sort_by_key(|&i| at_us(doc, &hits[i]));
    for &copy in &order {
        if !RELAYABLE.contains(&hits[copy].rule) || hits[copy].relays.is_some() {
            continue;
        }
        let Some(copy_msg) = anchor(doc, &hits[copy]) else { continue };
        let origin = order
            .iter()
            .copied()
            .filter(|&o| o != copy && !paired.contains(&o) && hits[o].relays.is_none())
            .filter(|&o| forwards(doc, &hits[o], &hits[copy], copy_msg))
            .max_by_key(|&o| at_us(doc, &hits[o]));
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

/// Whether `copy` (anchored on `copy_msg`) forwards `origin`: same rule, on
/// another leg, a different emitter, taken by the copy's emitting host before
/// the copy went out, and one message carried on.
fn forwards(doc: &FlowsDoc, origin: &Hit, copy: &Hit, copy_msg: &MsgJson) -> bool {
    if origin.rule != copy.rule || origin.leg == copy.leg || origin.emitter == copy.emitter {
        return false;
    }
    let Some(origin_msg) = anchor(doc, origin) else { return false };
    let same_host = match (endpoint_ip(&origin_msg.dst), endpoint_ip(&copy_msg.src)) {
        (Some(took), Some(sent)) => took == sent,
        _ => origin_msg.dst == copy_msg.src,
    };
    same_host
        && origin_msg.ts_us <= copy_msg.ts_us
        && same_kind(origin_msg, copy_msg)
        && same_description(origin_msg, copy_msg)
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
/// the same bytes, or the same origin session id and version (RFC 4566 §5.2),
/// which names one version of one session whatever a forwarding hop rewrote
/// around it.
fn same_description(a: &MsgJson, b: &MsgJson) -> bool {
    match (body_of(a), body_of(b)) {
        (None, None) => true,
        (Some(x), Some(y)) if x == y => true,
        (Some(x), Some(y)) => {
            match (sip_message::sdp_doc::parse_origin(&x), sip_message::sdp_doc::parse_origin(&y)) {
                (Some(ox), Some(oy)) => {
                    ox.session_id == oy.session_id && ox.session_version == oy.session_version
                }
                _ => false,
            }
        }
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
}
