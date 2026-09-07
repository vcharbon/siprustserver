//! Conformance pins for the merged PRACK rule (`rfc_rules::rules::prack`),
//! run through the capture adapter: the corpus-derived semantics of
//! `unacked-reliable-provisional`, decided off emitted flows documents.

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::super::{detect, scan, Evidence, RfcRule};

    const REL_180: &str = "Require: 100rel\r\nRSeq: 1\r\n";
    const OFFER: &str = "Supported: 100rel\r\n";

    /// A conversation long enough that the PRACK window closes inside it: the
    /// callee rings reliably at 1 s and the call runs on to 30 s.
    fn ladder(call: &str, prack: Option<&str>, tail: Vec<crate::Datagram>) -> Vec<crate::Datagram> {
        let mut out = vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, call, "fa", None, OFFER)),
            dg(1_100_000, B, A, response(100, "Trying", 1, "INVITE", call, "fa", None)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", call, "fa", Some("tb"), REL_180),
            ),
        ];
        if let Some(extra) = prack {
            out.push(dg(1_300_000, A, B, request_hdr("PRACK", 2, call, "fa", Some("tb"), extra)));
            out.push(dg(1_350_000, B, A, response(200, "OK", 2, "PRACK", call, "fa", Some("tb"))));
        }
        out.extend(tail);
        out
    }

    /// The established happy tail: answered at 10 s, hung up at 30 s.
    fn answered(call: &str) -> Vec<crate::Datagram> {
        vec![
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", call, "fa", Some("tb"))),
            dg(10_100_000, A, B, request("ACK", 1, call, "fa", Some("tb"))),
            dg(30_000_000, A, B, request("BYE", 3, call, "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 3, "BYE", call, "fa", Some("tb"))),
        ]
    }

    /// The violation: 100rel negotiated, a reliable 180 taken, no PRACK ever,
    /// and nine seconds of live dialog in which to have sent one.
    #[test]
    fn a_uac_that_never_pracks_a_reliable_provisional_is_a_violation() {
        let hits = detect(&doc_of(ladder("u1", None, answered("u1"))));
        assert_eq!(hits.len(), 1, "one violation, at the UAC: {hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.rule, RfcRule::UnackedReliableProvisional);
        assert_eq!(
            serde_json::to_value(hit.rule).unwrap(),
            serde_json::json!("unacked-reliable-provisional"),
            "the report spells the rule the way the replay vocabulary does"
        );
        assert_eq!(hit.emitter, A, "the UAC owed the PRACK");
        assert_eq!(hit.taker, B, "the UAS was owed it");
        assert_eq!(hit.cseq, 1);
        assert!(!hit.relayed);
        let Evidence::Unacked { rseq, status, window_us, provisional_ts_us, .. } = hit.evidence
        else {
            panic!("the prack rule carries its own evidence: {hit:?}")
        };
        assert_eq!((rseq, status), (1, 180));
        assert_eq!(provisional_ts_us, 1_200_000);
        assert_eq!(window_us, 8_800_000, "released by the 200 at 10 s");
    }

    /// The compliant path, and the RED PROOF for the one above: the SAME
    /// ladder with a PRACK whose RAck names the provisional. Nothing else
    /// changes, and the hit disappears.
    #[test]
    fn a_provisional_the_uac_pracked_is_not_a_violation() {
        let scanned = scan(&doc_of(ladder("u2", Some("RAck: 1 1 INVITE\r\n"), answered("u2"))));
        assert!(scanned.hits.is_empty(), "the PRACK answers it: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 1), "an obligation met is still an occasion");
    }

    /// A PRACK is matched on the WHOLE RAck, not on its response-num: an RAck
    /// naming the right RSeq of the wrong transaction answers nothing.
    #[test]
    fn a_prack_whose_rack_names_another_transaction_answers_nothing() {
        let hits = detect(&doc_of(ladder("u3", Some("RAck: 1 9 INVITE\r\n"), answered("u3"))));
        assert_eq!(hits.len(), 1, "the CSeq number in the RAck does not match: {hits:?}");
    }

    /// The negotiation is what makes a provisional reliable. An `RSeq` with no
    /// `Require: 100rel` beside it is not a reliable provisional at all, so
    /// there is no obligation and not even an occasion.
    #[test]
    fn an_rseq_without_the_100rel_requirement_is_not_a_reliable_provisional() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "n1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "n1", "fa", Some("tb"), "RSeq: 1\r\n"),
            ),
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", "n1", "fa", Some("tb"))),
            dg(10_100_000, A, B, request("ACK", 1, "n1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "no Require: 100rel, no obligation: {:?}", scanned.hits);
        assert!(!scanned.population.contains_key("unacked-reliable-provisional"));
    }

    /// A UAC that never offered `100rel` cannot be held to RFC 3262 §4: the
    /// UAS sending reliably anyway is the UAS's business. The occasion is
    /// counted and left UNDECIDED, so the report shows what the gate cost.
    #[test]
    fn a_provisional_whose_negotiation_this_vantage_never_saw_is_undecided() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "g1", "fa", None)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(183, "Progress", 1, "INVITE", "g1", "fa", Some("tb"), REL_180),
            ),
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", "g1", "fa", Some("tb"))),
            dg(10_100_000, A, B, request("ACK", 1, "g1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "no 100rel offer was witnessed: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 0));
    }

    /// The dialog dies before the window closes: a 486 four hundred
    /// milliseconds after the reliable provisional. A PRACK may have been in
    /// flight, so the occasion is undecided rather than charged.
    #[test]
    fn a_transaction_that_ended_inside_the_prack_window_is_not_charged() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "e1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "e1", "fa", Some("tb"), REL_180),
            ),
            dg(1_600_000, B, A, response(486, "Busy Here", 1, "INVITE", "e1", "fa", Some("tb"))),
            dg(1_610_000, A, B, request("ACK", 1, "e1", "fa", Some("tb"))),
            dg(9_000_000, A, B, request_hdr("INVITE", 5, "e1", "fa", None, OFFER)),
            dg(9_100_000, B, A, response(486, "Busy Here", 5, "INVITE", "e1", "fa", Some("tb"))),
            dg(9_110_000, A, B, request("ACK", 5, "e1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "released 400 ms in: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 0));
    }

    /// The UAC's own CANCEL releases it the same way a final does.
    #[test]
    fn a_uac_that_cancels_inside_the_window_is_not_charged() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "k1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "k1", "fa", Some("tb"), REL_180),
            ),
            dg(1_500_000, A, B, request("CANCEL", 1, "k1", "fa", None)),
            dg(1_550_000, B, A, response(200, "OK", 1, "CANCEL", "k1", "fa", Some("tb"))),
            dg(
                1_600_000,
                B,
                A,
                response(487, "Request Terminated", 1, "INVITE", "k1", "fa", Some("tb")),
            ),
            dg(1_650_000, A, B, request("ACK", 1, "k1", "fa", Some("tb"))),
            dg(9_000_000, A, B, request_hdr("INVITE", 5, "k1", "fa", None, OFFER)),
            dg(9_100_000, B, A, response(486, "Busy Here", 5, "INVITE", "k1", "fa", Some("tb"))),
            dg(9_110_000, A, B, request("ACK", 5, "k1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "the UAC gave up 300 ms in: {:?}", scanned.hits);
    }

    /// A capture that stops inside the window proves nothing: the missing
    /// PRACK is missing from the RECORDING, not demonstrably from the wire.
    #[test]
    fn a_capture_that_ends_inside_the_window_is_not_charged() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "s1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "s1", "fa", Some("tb"), REL_180),
            ),
            dg(
                1_700_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "s1", "fa", Some("tb"), REL_180),
            ),
        ]));
        assert!(scanned.hits.is_empty(), "the recording stopped 500 ms in: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 0), "one obligation, one RSeq, undecided");
    }

    /// A PRACK whose RAck will not parse makes that emitter's obligations
    /// undecidable: the unreadable one may be exactly the missing answer.
    #[test]
    fn an_unreadable_rack_leaves_the_emitters_obligations_undecided() {
        let scanned =
            scan(&doc_of(ladder("b1", Some("RAck: not-a-number\r\n"), answered("b1"))));
        assert!(scanned.hits.is_empty(), "the PRACK might be the one: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 0));
    }

    /// Two reliable provisionals, two RSeqs, two obligations — and a PRACK
    /// that answers only the first. RFC 3262 §3 numbers them independently.
    #[test]
    fn each_rseq_is_its_own_obligation() {
        let hits = detect(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "m1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "m1", "fa", Some("tb"), REL_180),
            ),
            dg(1_300_000, A, B, request_hdr("PRACK", 2, "m1", "fa", Some("tb"), "RAck: 1 1 INVITE\r\n")),
            dg(1_350_000, B, A, response(200, "OK", 2, "PRACK", "m1", "fa", Some("tb"))),
            dg(
                2_000_000,
                B,
                A,
                response_hdr(
                    183,
                    "Progress",
                    1,
                    "INVITE",
                    "m1",
                    "fa",
                    Some("tb"),
                    "Require: 100rel\r\nRSeq: 2\r\n",
                ),
            ),
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", "m1", "fa", Some("tb"))),
            dg(10_100_000, A, B, request("ACK", 1, "m1", "fa", Some("tb"))),
        ]));
        assert_eq!(hits.len(), 1, "only RSeq 2 went unanswered: {hits:?}");
        let Evidence::Unacked { rseq, status, .. } = hits[0].evidence else { panic!("prack") };
        assert_eq!((rseq, status), (2, 183));
    }

    /// A repeat of one reliable provisional is one obligation: the UAS
    /// retransmits until the PRACK arrives (§3), and one PRACK answers them
    /// all — so a report must not count the ladder as a ladder of violations.
    #[test]
    fn repeats_of_one_reliable_provisional_are_one_obligation() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "p1", "fa", None, OFFER)),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "p1", "fa", Some("tb"), REL_180),
            ),
            dg(
                1_700_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "p1", "fa", Some("tb"), REL_180),
            ),
            dg(
                2_700_000,
                B,
                A,
                response_hdr(180, "Ringing", 1, "INVITE", "p1", "fa", Some("tb"), REL_180),
            ),
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", "p1", "fa", Some("tb"))),
            dg(10_100_000, A, B, request("ACK", 1, "p1", "fa", Some("tb"))),
        ]));
        assert_eq!(scanned.hits.len(), 1, "three datagrams, one RSeq: {:?}", scanned.hits);
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!((p.occasions, p.decided), (1, 1));
    }

    /// The corpus shape, and the reason a hop is not a UAC: one Call-ID across
    /// a proxy that does NOT record-route, captured on both of its wires. The
    /// caller PRACKs the far UAS DIRECTLY, so the proxy never sees the PRACK
    /// for a reliable provisional it demonstrably relayed. Charging the proxy
    /// would charge the one box on the wire that owes nothing.
    fn through_a_proxy(call: &str, prack: bool) -> Vec<crate::Datagram> {
        let rel = "Require: 100rel\r\nRSeq: 887672120\r\n";
        let mut out = vec![
            dg(0, A, P, request_hdr("INVITE", 1, call, "fa", None, OFFER)),
            dg(1_000, P, A, response(100, "Trying", 1, "INVITE", call, "fa", None)),
            dg(53_000, P, B, request_hdr("INVITE", 1, call, "fa", None, OFFER)),
            dg(56_000, B, P, response(100, "Trying", 1, "INVITE", call, "fa", None)),
            dg(280_000, B, P, response_hdr(180, "Ringing", 1, "INVITE", call, "fa", Some("tb"), rel)),
        ];
        if prack {
            out.push(dg(
                295_000,
                A,
                B,
                request_hdr("PRACK", 2, call, "fa", Some("tb"), "RAck: 887672120 1 INVITE\r\n"),
            ));
        }
        out.push(dg(297_000, P, A, response_hdr(180, "Ringing", 1, "INVITE", call, "fa", Some("tb"), rel)));
        if prack {
            out.push(dg(343_000, B, A, response(200, "OK", 2, "PRACK", call, "fa", Some("tb"))));
        }
        out.extend([
            dg(7_640_000, B, P, response(200, "OK", 1, "INVITE", call, "fa", Some("tb"))),
            dg(7_659_000, P, A, response(200, "OK", 1, "INVITE", call, "fa", Some("tb"))),
            dg(7_661_000, A, B, request("ACK", 1, call, "fa", Some("tb"))),
            dg(13_747_000, B, A, request("BYE", 9, call, "fa", Some("tb"))),
            dg(13_764_000, A, B, response(200, "OK", 9, "BYE", call, "fa", Some("tb"))),
        ]);
        out
    }

    #[test]
    fn a_hop_that_relayed_the_invite_owes_no_prack() {
        let scanned = scan(&doc_of(through_a_proxy("px", true)));
        assert!(
            scanned.hits.is_empty(),
            "the UAC PRACKed direct and the proxy owes nothing: {:?}",
            scanned.hits
        );
        let p = scanned.population["unacked-reliable-provisional"];
        assert_eq!(
            (p.occasions, p.decided),
            (2, 2),
            "one RSeq, acknowledged on the leg: both takers' obligations are met"
        );
    }

    /// The RED PROOF for the gate above: the SAME ladder with the PRACK
    /// removed. Exactly one endpoint is charged — the UAC that opened the
    /// INVITE — and the proxy that relayed it is still not.
    #[test]
    fn the_uac_behind_a_relaying_hop_is_the_one_charged() {
        let hits = detect(&doc_of(through_a_proxy("py", false)));
        assert_eq!(hits.len(), 1, "one obligation, one holder: {hits:?}");
        assert_eq!(hits[0].emitter, A, "the UAC opened the INVITE");
        assert_eq!(hits[0].taker, P, "it took the provisional from the hop in front of it");
        assert!(!hits[0].relayed);
        let Evidence::Unacked { window_us, rseq, .. } = hits[0].evidence else { panic!("prack") };
        assert_eq!((rseq, window_us), (887_672_120, 7_362_000));
    }

    /// The corpus's second hop shape, and the reason the obligation is keyed on
    /// the DIALOG: the vantage carries the proxy's downstream wire and the
    /// UAC's direct in-dialog wire, but not the hop between UAC and proxy. So
    /// the proxy LOOKS like the endpoint that opened the INVITE, and the only
    /// thing that says otherwise is the PRACK — sent by a third socket, naming
    /// the exact RSeq, on the same leg.
    fn past_the_proxy(call: &str, prack: bool) -> Vec<crate::Datagram> {
        let rel = "Require: 100rel\r\nRSeq: 604864145\r\n";
        let mut out = vec![
            dg(0, P, B, request_hdr("INVITE", 1, call, "fa", None, OFFER)),
            dg(1_000, B, P, response(100, "Trying", 1, "INVITE", call, "fa", None)),
            dg(330_000, B, P, response_hdr(180, "Ringing", 1, "INVITE", call, "fa", Some("tb"), rel)),
        ];
        if prack {
            out.push(dg(
                346_000,
                A,
                B,
                request_hdr("PRACK", 2, call, "fa", Some("tb"), "RAck: 604864145 1 INVITE\r\n"),
            ));
            out.push(dg(368_000, B, A, response(200, "OK", 2, "PRACK", call, "fa", Some("tb"))));
        }
        out.extend([
            dg(1_878_000, B, P, response(200, "OK", 1, "INVITE", call, "fa", Some("tb"))),
            dg(1_901_000, A, B, request("ACK", 1, call, "fa", Some("tb"))),
            dg(50_098_000, A, B, request("BYE", 9, call, "fa", Some("tb"))),
            dg(50_114_000, B, A, response(200, "OK", 9, "BYE", call, "fa", Some("tb"))),
        ]);
        out
    }

    #[test]
    fn an_rseq_another_socket_pracked_on_the_leg_is_answered() {
        let scanned = scan(&doc_of(past_the_proxy("vx", true)));
        assert!(
            scanned.hits.is_empty(),
            "the RSeq was acknowledged on this dialog: {:?}",
            scanned.hits
        );
    }

    /// The RED PROOF, and the detector's remaining vantage limit stated as a
    /// test: with the PRACK gone there is nothing left on the wire that says
    /// the proxy is a hop, so the hop is charged. `emitter_role` and the
    /// registry's SUT-side guard are what a human rules on next.
    #[test]
    fn without_the_prack_the_only_visible_opener_is_charged() {
        let hits = detect(&doc_of(past_the_proxy("vy", false)));
        assert_eq!(hits.len(), 1, "one taker, and nothing says it is a hop: {hits:?}");
        assert_eq!(hits[0].emitter, P);
    }

    /// A parallel fork: two UASes answer one INVITE and both number their
    /// first reliable provisional `RSeq: 1`. The early dialog's To tag is what
    /// keeps the two obligations apart (RFC 3262 §4 PRACKs a DIALOG), so the
    /// PRACK that answered the first fork answers nothing of the second's.
    #[test]
    fn two_forks_that_chose_the_same_rseq_are_two_obligations() {
        const FORK: &str = "10.0.0.4:5060";
        let rel = "Require: 100rel\r\nRSeq: 1\r\n";
        let hits = detect(&doc_of(vec![
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "f1", "fa", None, OFFER)),
            dg(1_200_000, B, A, response_hdr(183, "Progress", 1, "INVITE", "f1", "fa", Some("t1"), rel)),
            dg(1_300_000, A, B, request_hdr("PRACK", 2, "f1", "fa", Some("t1"), "RAck: 1 1 INVITE\r\n")),
            dg(1_350_000, B, A, response(200, "OK", 2, "PRACK", "f1", "fa", Some("t1"))),
            dg(1_400_000, FORK, A, response_hdr(183, "Progress", 1, "INVITE", "f1", "fa", Some("t2"), rel)),
            dg(10_000_000, B, A, response(200, "OK", 1, "INVITE", "f1", "fa", Some("t1"))),
            dg(10_100_000, A, B, request("ACK", 1, "f1", "fa", Some("t1"))),
        ]));
        assert_eq!(hits.len(), 1, "the second fork's RSeq 1 went unanswered: {hits:?}");
        assert_eq!(hits[0].taker, FORK);
        let Evidence::Unacked { rseq, .. } = hits[0].evidence else { panic!("prack") };
        assert_eq!(rseq, 1, "same number, different dialog, different obligation");
    }
}
