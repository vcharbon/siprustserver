//! Conformance pins for the merged CANCEL rule (`rfc_rules::rules::cancel`),
//! run through the capture adapter: the corpus-derived semantics of
//! `no-200-after-cancel`, decided off emitted flows documents.

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::super::{detect, scan, EndpointRole, Evidence, RfcRule};

    /// The violation: the callee takes the CANCEL and answers 200 anyway.
    #[test]
    fn a_uas_that_answers_200_after_taking_the_cancel_is_a_violation() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "v1", "fa", None)),
            dg(2_000, B, A, response(100, "Trying", 1, "INVITE", "v1", "fa", None)),
            dg(3_000, B, A, response(180, "Ringing", 1, "INVITE", "v1", "fa", Some("tb"))),
            dg(4_000, A, B, request("CANCEL", 1, "v1", "fa", None)),
            dg(5_000, B, A, response(200, "OK", 1, "CANCEL", "v1", "fa", Some("tb"))),
            dg(6_000, B, A, response(200, "OK", 1, "INVITE", "v1", "fa", Some("tb"))),
            dg(7_000, A, B, request("ACK", 1, "v1", "fa", Some("tb"))),
        ]));
        assert_eq!(hits.len(), 1, "one violation, at the callee: {hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.rule, RfcRule::No200AfterCancel);
        assert_eq!(
            serde_json::to_value(hit.rule).unwrap(),
            serde_json::json!("no-200-after-cancel"),
            "the report spells the rule the way the replay vocabulary does"
        );
        assert_eq!(hit.emitter, B, "the UAS that answered is the emitter");
        assert_eq!(hit.taker, A);
        assert_eq!(hit.cseq, 1);
        let Evidence::Cancelled { cancel_ts_us, response_ts_us, status, gap_us, .. } = hit.evidence
        else {
            panic!("the cancel rule carries its own evidence: {hit:?}")
        };
        assert_eq!((cancel_ts_us, response_ts_us), (4_000, 6_000));
        assert_eq!(status, 200);
        assert_eq!(gap_us, 2_000);
        assert!(!hit.relayed, "the callee terminated the INVITE, it forwarded nothing");
        // The flattened evidence keeps a hit ONE flat object, so the report's
        // field names are the ones the census README documents.
        let json = serde_json::to_value(hit).unwrap();
        assert_eq!(json["gap_us"], 2_000);
        assert_eq!(json["response_msg"], 5);
    }

    /// The legitimate race: the 200 was already on the wire when the CANCEL
    /// arrived. Compliant — the UAS had nothing to answer 487 to.
    #[test]
    fn a_200_that_crossed_the_cancel_in_flight_is_not_a_violation() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "x1", "fa", None)),
            dg(3_000, B, A, response(180, "Ringing", 1, "INVITE", "x1", "fa", Some("tb"))),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "x1", "fa", Some("tb"))),
            dg(4_500, A, B, request("CANCEL", 1, "x1", "fa", None)),
            dg(5_000, B, A, response(481, "No Transaction", 1, "CANCEL", "x1", "fa", Some("tb"))),
            dg(6_000, A, B, request("ACK", 1, "x1", "fa", Some("tb"))),
            dg(7_000, A, B, request("BYE", 2, "x1", "fa", Some("tb"))),
            dg(8_000, B, A, response(200, "OK", 2, "BYE", "x1", "fa", Some("tb"))),
        ]));
        assert!(hits.is_empty(), "a crossing is compliant behaviour: {hits:?}");
    }

    /// The denominators the count has to be read against: a crossing is one
    /// occasion the capture DECIDED and no hit, so a report can say how rare
    /// the violation is among the calls that could have produced it.
    #[test]
    fn a_crossing_is_counted_as_a_decided_occasion_but_not_a_hit() {
        let crossed = scan(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "d1", "fa", None)),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "d1", "fa", Some("tb"))),
            dg(4_500, A, B, request("CANCEL", 1, "d1", "fa", None)),
            dg(6_000, A, B, request("ACK", 1, "d1", "fa", Some("tb"))),
        ]));
        let p = crossed.population["no-200-after-cancel"];
        assert_eq!((p.occasions, p.decided, crossed.hits.len()), (1, 1, 0));

        let obeyed = scan(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "d2", "fa", None)),
            dg(4_000, A, B, request("CANCEL", 1, "d2", "fa", None)),
            dg(4_500, B, A, response(487, "Request Terminated", 1, "INVITE", "d2", "fa", Some("tb"))),
            dg(5_000, A, B, request("ACK", 1, "d2", "fa", Some("tb"))),
        ]));
        let p = obeyed.population["no-200-after-cancel"];
        assert_eq!(
            (p.occasions, p.decided, obeyed.hits.len()),
            (1, 1, 0),
            "a CANCEL obeyed is an occasion the capture decided"
        );
    }

    /// The compliant path: CANCEL taken, 487 answered. Nothing to report.
    #[test]
    fn a_uas_that_answers_487_after_the_cancel_is_compliant() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "c1", "fa", None)),
            dg(3_000, B, A, response(180, "Ringing", 1, "INVITE", "c1", "fa", Some("tb"))),
            dg(4_000, A, B, request("CANCEL", 1, "c1", "fa", None)),
            dg(5_000, B, A, response(200, "OK", 1, "CANCEL", "c1", "fa", Some("tb"))),
            dg(5_500, B, A, response(487, "Request Terminated", 1, "INVITE", "c1", "fa", Some("tb"))),
            dg(6_000, A, B, request("ACK", 1, "c1", "fa", Some("tb"))),
        ]));
        assert!(hits.is_empty(), "487 is what §9.2 asks for: {hits:?}");
    }

    /// The two-key proof, half one. The callee's OWN re-INVITE carries CSeq
    /// number 1 — the two directions of a dialog number independently — and the
    /// caller answers it 200 after a CANCEL was seen on this leg. Different
    /// transaction, so no violation: the CANCEL went TO the callee, and the
    /// 200 came FROM the caller.
    #[test]
    fn a_re_invite_answered_after_a_cancel_of_the_initial_invite_is_a_different_transaction() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "r1", "fa", None)),
            dg(3_000, B, A, response(180, "Ringing", 1, "INVITE", "r1", "fa", Some("tb"))),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"))),
            dg(4_500, A, B, request("ACK", 1, "r1", "fa", Some("tb"))),
            // A stray CANCEL for the initial INVITE reaches the callee after it
            // answered — the crossing shape, and the callee rejects it.
            dg(5_000, A, B, request("CANCEL", 1, "r1", "fa", None)),
            dg(5_200, B, A, response(481, "No Transaction", 1, "CANCEL", "r1", "fa", Some("tb"))),
            // The callee now re-INVITEs, numbering its own CSeq space from 1.
            dg(6_000, B, A, request("INVITE", 1, "r1", "fa", Some("tb"))),
            dg(7_000, A, B, response(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"))),
            dg(7_500, B, A, request("ACK", 1, "r1", "fa", Some("tb"))),
        ]));
        // The stray CANCEL leaves after this emitter ACKed the 200, so
        // `no-cancel-after-final` charges it on its own account; what this test
        // pins is the two-key reading of the rule this module is about.
        let crossed: Vec<_> =
            hits.iter().filter(|h| h.rule == RfcRule::No200AfterCancel).collect();
        assert!(
            crossed.is_empty(),
            "the CSeq number collides but the direction does not: {hits:?}"
        );
    }

    /// The two-key proof, half two — the SAME document with the CANCEL's
    /// direction flipped, so it really does cancel the re-INVITE. Now the
    /// caller answered 200 to a transaction it had taken the CANCEL for, and
    /// the detector fires. Direction is what separates this from the case
    /// above; nothing else changed.
    #[test]
    fn the_same_cseq_number_with_the_cancel_the_other_way_round_is_a_violation() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "r2", "fa", None)),
            dg(3_000, B, A, response(180, "Ringing", 1, "INVITE", "r2", "fa", Some("tb"))),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "r2", "fa", Some("tb"))),
            dg(4_500, A, B, request("ACK", 1, "r2", "fa", Some("tb"))),
            dg(6_000, B, A, request("INVITE", 1, "r2", "fa", Some("tb"))),
            // The callee gives up on its own re-INVITE and cancels it.
            dg(6_500, B, A, request("CANCEL", 1, "r2", "fa", Some("tb"))),
            dg(6_700, A, B, response(200, "OK", 1, "CANCEL", "r2", "fa", Some("tb"))),
            dg(7_000, A, B, response(200, "OK", 1, "INVITE", "r2", "fa", Some("tb"))),
        ]));
        assert_eq!(hits.len(), 1, "the caller broke §9.2 on the re-INVITE: {hits:?}");
        assert_eq!(hits[0].emitter, A);
        let Evidence::Cancelled { cancel_ts_us, response_ts_us, .. } = hits[0].evidence else {
            panic!("cancel evidence")
        };
        assert_eq!((cancel_ts_us, response_ts_us), (6_500, 7_000));
    }

    /// A 2xx retransmitted after the CANCEL arrived is not a fresh violation:
    /// the answer crossed, and RFC 3261 §13.3.1.4 obliges the UAS to repeat it
    /// until it is ACKed.
    #[test]
    fn a_retransmitted_200_that_first_went_out_before_the_cancel_is_not_a_violation() {
        let hits = detect(&doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "t1", "fa", None)),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "t1", "fa", Some("tb"))),
            dg(4_500, A, B, request("CANCEL", 1, "t1", "fa", None)),
            dg(5_000, B, A, response(481, "No Transaction", 1, "CANCEL", "t1", "fa", Some("tb"))),
            dg(504_000, B, A, response(200, "OK", 1, "INVITE", "t1", "fa", Some("tb"))),
            dg(505_000, A, B, request("ACK", 1, "t1", "fa", Some("tb"))),
        ]));
        assert!(hits.is_empty(), "a required repeat is not a fresh answer: {hits:?}");
    }

    /// Emitter attribution across a B2BUA: the platform endpoint is on both
    /// legs, the far callee on one, and the violation is the callee's — which
    /// is what decides whether a generated document would gate on it.
    #[test]
    fn the_emitter_is_attributed_to_the_side_of_the_deployment_it_sits_on() {
        let hits = detect(&doc_of(vec![
            // a-leg: caller ↔ platform.
            dg(1_000, A, P, request("INVITE", 1, "leg-a", "fa", None)),
            dg(1_100, P, A, response(100, "Trying", 1, "INVITE", "leg-a", "fa", None)),
            // b-leg: platform ↔ callee, tied by the relayed X-Api-Call token.
            dg(1_200, P, B, request_tok("INVITE", 1, "leg-b", "fp", None, "leg-a")),
            dg(2_000, B, P, response(180, "Ringing", 1, "INVITE", "leg-b", "fp", Some("tb"))),
            dg(3_000, A, P, request("CANCEL", 1, "leg-a", "fa", None)),
            dg(3_100, P, A, response(200, "OK", 1, "CANCEL", "leg-a", "fa", None)),
            dg(3_200, P, B, request("CANCEL", 1, "leg-b", "fp", None)),
            dg(3_300, B, P, response(200, "OK", 1, "CANCEL", "leg-b", "fp", Some("tb"))),
            dg(4_000, B, P, response(200, "OK", 1, "INVITE", "leg-b", "fp", Some("tb"))),
            dg(4_500, P, B, request("ACK", 1, "leg-b", "fp", Some("tb"))),
            dg(5_000, P, A, response(487, "Request Terminated", 1, "INVITE", "leg-a", "fa", Some("tp"))),
            dg(5_100, A, P, request("ACK", 1, "leg-a", "fa", Some("tp"))),
        ]));
        assert_eq!(hits.len(), 1, "only the callee broke the rule: {hits:?}");
        assert_eq!(hits[0].emitter, B);
        assert_eq!(hits[0].emitter_role, EndpointRole::Peer);
        assert!(!hits[0].relayed);
    }

    /// The corpus shape: one Call-ID crossing a proxy, captured on both of its
    /// wires. The far UAS ORIGINATES the violation; the proxy in the middle,
    /// which had already forwarded the CANCEL, then relays that same 200
    /// upstream. Both are hits and the report tells them apart — and the proxy
    /// wears a different address on each interface, so nothing may key relay
    /// detection on the emitter's own address.
    #[test]
    fn a_relayed_200_is_reported_as_a_relay_and_the_originator_as_the_originator() {
        const FAR: &str = "10.0.0.3:5060";
        const PROXY_NEAR: &str = "10.0.0.8:5060";
        const PROXY_FAR: &str = "10.0.0.7:5060";
        let hits = detect(&doc_of(vec![
            dg(1_000, A, PROXY_NEAR, request("INVITE", 7, "chain", "fa", None)),
            dg(1_200, PROXY_FAR, FAR, request("INVITE", 7, "chain", "fa", None)),
            dg(2_000, FAR, PROXY_FAR, response(180, "Ringing", 7, "INVITE", "chain", "fa", Some("tz"))),
            dg(3_000, A, PROXY_NEAR, request("CANCEL", 7, "chain", "fa", None)),
            dg(3_200, PROXY_FAR, FAR, request("CANCEL", 7, "chain", "fa", None)),
            // The far UAS answers 200 anyway: the violation, originated.
            dg(4_000, FAR, PROXY_FAR, response(200, "OK", 7, "INVITE", "chain", "fa", Some("tz"))),
            // The proxy forwards it on, having already passed the CANCEL down.
            dg(4_400, PROXY_NEAR, A, response(200, "OK", 7, "INVITE", "chain", "fa", Some("tz"))),
            dg(5_000, A, PROXY_NEAR, request("ACK", 7, "chain", "fa", Some("tz"))),
            dg(5_200, PROXY_FAR, FAR, request("ACK", 7, "chain", "fa", Some("tz"))),
        ]));
        assert_eq!(hits.len(), 2, "the UAS and the proxy that forwarded it: {hits:?}");
        let origin = hits.iter().find(|h| h.emitter == FAR).expect("the far UAS is a hit");
        assert!(!origin.relayed, "the far UAS originated this 200");
        let relay = hits.iter().find(|h| h.emitter == PROXY_NEAR).expect("the proxy is a hit");
        assert!(relay.relayed, "the proxy forwarded a 200 it had already been sent");
        assert_eq!(relay.taker, A);
    }
}
