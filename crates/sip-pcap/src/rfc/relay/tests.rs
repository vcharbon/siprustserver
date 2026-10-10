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
        dg(1_200, P, B, with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer)),
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
        dg(3_100, P, B, with_body(request("ACK", 1, "leg-b", "fp", Some("tb")), relayed_ack_body)),
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
        dg(1_200, P, B, with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer)),
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
        dg(1_200, P, B, with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer)),
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
            dg(5_000, A, P, with_body(request("INVITE", 2, "leg-a", "fa", Some("tp")), &reoffer)),
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
                with_body(response(200, "OK", 2, "INVITE", "leg-b", "fp", Some("tb")), &reanswer),
            ),
            dg(5_250, P, B, request("ACK", 2, "leg-b", "fp", Some("tb"))),
            dg(
                5_300,
                P,
                A,
                with_body(response(200, "OK", 2, "INVITE", "leg-a", "fa", Some("tp")), &reanswer),
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
        dg(1_200, P, B, with_body(request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a"), &offer)),
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
