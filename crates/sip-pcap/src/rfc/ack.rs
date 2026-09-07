//! Conformance pins for the merged ACK-pair rule (`rfc_rules::rules::ack`),
//! run through the capture adapter: the corpus-derived semantics of
//! `no-ack-to-dialog-creating-2xx`, decided off emitted flows documents.

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::super::{detect, scan, Evidence, RfcRule};

    /// The happy shape this rule measures against: a call answered at 1.2 s,
    /// ACKed, and hung up at 30 s.
    fn call(id: &str, ack: bool) -> Vec<crate::Datagram> {
        let mut out = vec![
            dg(1_000_000, A, B, request("INVITE", 1, id, "fa", None)),
            dg(1_100_000, B, A, response(100, "Trying", 1, "INVITE", id, "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", id, "fa", Some("tb"))),
        ];
        if ack {
            out.push(dg(1_250_000, A, B, request("ACK", 1, id, "fa", Some("tb"))));
        }
        out.extend([
            dg(30_000_000, A, B, request("BYE", 2, id, "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 2, "BYE", id, "fa", Some("tb"))),
        ]);
        out
    }

    /// The violation: the UAC takes the 2xx that confirms its own dialog, never
    /// ACKs it, and the recording runs on for another half minute.
    #[test]
    fn a_uac_that_never_acks_a_dialog_creating_2xx_is_a_violation() {
        let hits = detect(&doc_of(call("u1", false)));
        assert_eq!(hits.len(), 1, "one violation, at the UAC: {hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.rule, RfcRule::NoAckToDialogCreating2xx);
        assert_eq!(
            serde_json::to_value(hit.rule).unwrap(),
            serde_json::json!("no-ack-to-dialog-creating-2xx"),
            "the report spells the rule the way the replay vocabulary does"
        );
        assert_eq!(hit.emitter, A, "the UAC owed the ACK");
        assert_eq!(hit.taker, B, "the UAS was owed it");
        assert_eq!(hit.cseq, 1);
        assert!(!hit.relayed);
        let Evidence::NoAck {
            to_tag, status, window_us, emitter_window_us, retransmits, bye_after_us, bye_by, ..
        } = &hit.evidence
        else {
            panic!("the ack rule carries its own evidence: {hit:?}")
        };
        assert_eq!((to_tag.as_str(), *status, *retransmits), ("tb", 200, 0));
        assert_eq!(*window_us, 28_900_000, "the recording ran to the BYE's 200");
        assert_eq!(*emitter_window_us, 28_900_000, "and it carried the UAC's traffic throughout");
        assert_eq!(*bye_after_us, Some(28_800_000));
        assert_eq!(bye_by.as_deref(), Some(A), "the un-ACKed dialog was torn down anyway");
    }

    /// The compliant path, and the RED PROOF for the one above: the SAME ladder
    /// with the ACK on board. Nothing else changes, and the hit disappears.
    #[test]
    fn a_2xx_the_uac_acked_is_not_a_violation() {
        let scanned = scan(&doc_of(call("u2", true)));
        assert!(scanned.hits.is_empty(), "the ACK answers it: {:?}", scanned.hits);
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!((p.occasions, p.decided), (1, 1), "an obligation met is still an occasion");
    }

    /// An ACK is matched on the WHOLE key: an ACK naming another dialog's To
    /// tag answers nothing, whatever its CSeq.
    #[test]
    fn an_ack_naming_another_dialog_answers_nothing() {
        let hits = detect(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "u3", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "u3", "fa", Some("tb"))),
            dg(1_250_000, A, B, request("ACK", 1, "u3", "fa", Some("other"))),
            dg(30_000_000, A, B, request("BYE", 2, "u3", "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 2, "BYE", "u3", "fa", Some("tb"))),
        ]));
        assert_eq!(hits.len(), 1, "the To tag in the ACK does not match: {hits:?}");
    }

    /// A capture that stops inside the window proves nothing: the missing ACK
    /// is missing from the RECORDING, not demonstrably from the wire.
    #[test]
    fn a_capture_that_ends_inside_the_window_is_not_charged() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "s1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "s1", "fa", Some("tb"))),
            dg(1_700_000, B, A, response(200, "OK", 1, "INVITE", "s1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "the recording stopped 500 ms in: {:?}", scanned.hits);
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!((p.occasions, p.decided), (1, 0), "one obligation, undecided");
    }

    /// A 2xx to a RE-INVITE is answered inside a dialog that already exists.
    /// Its ACK is owed too, but under a different reading of §13, so this rule
    /// does not judge it and does not count it as one of its own occasions.
    #[test]
    fn a_2xx_to_a_re_invite_is_not_this_rules_business() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "r1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"))),
            dg(1_250_000, A, B, request("ACK", 1, "r1", "fa", Some("tb"))),
            dg(5_000_000, A, B, request("INVITE", 2, "r1", "fa", Some("tb"))),
            dg(5_100_000, B, A, response(200, "OK", 2, "INVITE", "r1", "fa", Some("tb"))),
            dg(30_000_000, A, B, request("BYE", 3, "r1", "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 3, "BYE", "r1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "the re-INVITE's 2xx is not judged: {:?}", scanned.hits);
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!((p.occasions, p.decided), (1, 1), "only the initial INVITE's 2xx");
    }

    /// An ACK the enricher flagged as a repeat is still an ACK on the wire. A
    /// vantage that caught only the repeat has caught the obligation met.
    #[test]
    fn a_repeated_ack_still_answers_the_obligation() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "p1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "p1", "fa", Some("tb"))),
            dg(1_250_000, A, B, request("ACK", 1, "p1", "fa", Some("tb"))),
            dg(1_700_000, B, A, response(200, "OK", 1, "INVITE", "p1", "fa", Some("tb"))),
            dg(1_750_000, A, B, request("ACK", 1, "p1", "fa", Some("tb"))),
            dg(30_000_000, A, B, request("BYE", 2, "p1", "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 2, "BYE", "p1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "the ladder was answered: {:?}", scanned.hits);
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!((p.occasions, p.decided), (1, 1), "two datagrams, one dialog, one obligation");
    }

    /// The UAS's own §13.3.1.4 ladder, unanswered: three deliveries of one 2xx
    /// are ONE obligation, and the rung count is what the report carries.
    #[test]
    fn the_uass_retransmission_ladder_is_one_obligation_with_a_count() {
        let hits = detect(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "l1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "l1", "fa", Some("tb"))),
            dg(1_700_000, B, A, response(200, "OK", 1, "INVITE", "l1", "fa", Some("tb"))),
            dg(2_700_000, B, A, response(200, "OK", 1, "INVITE", "l1", "fa", Some("tb"))),
            dg(33_200_000, B, A, request("BYE", 9, "l1", "tb", Some("fa"))),
            dg(33_300_000, A, B, response(200, "OK", 9, "BYE", "l1", "tb", Some("fa"))),
        ]));
        assert_eq!(hits.len(), 1, "three datagrams, one dialog: {hits:?}");
        let Evidence::NoAck { retransmits, bye_after_us, bye_by, .. } = &hits[0].evidence else {
            panic!("ack")
        };
        assert_eq!(*retransmits, 2, "two rungs beyond the first delivery");
        assert_eq!(*bye_after_us, Some(32_000_000), "the UAS gave up after 64*T1");
        assert_eq!(bye_by.as_deref(), Some(B), "and it is the UAS that gave up");
    }

    /// An ACK with no To tag names no dialog, so it may be the missing one.
    /// Every obligation of the endpoint that sent it becomes undecidable.
    #[test]
    fn an_ack_with_no_to_tag_leaves_the_emitters_obligations_undecided() {
        let scanned = scan(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "b1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "b1", "fa", Some("tb"))),
            dg(1_250_000, A, B, request("ACK", 1, "b1", "fa", None)),
            dg(30_000_000, A, B, request("BYE", 2, "b1", "fa", Some("tb"))),
            dg(30_100_000, B, A, response(200, "OK", 2, "BYE", "b1", "fa", Some("tb"))),
        ]));
        assert!(scanned.hits.is_empty(), "the untagged ACK might be the one: {:?}", scanned.hits);
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!((p.occasions, p.decided), (1, 0));
    }

    /// Two forks answer one INVITE and confirm two dialogs. §13.2.2.4 owes an
    /// ACK per 2xx received, so the ACK to the first fork answers nothing of
    /// the second's.
    #[test]
    fn two_forks_that_answered_are_two_obligations() {
        const FORK: &str = "10.0.0.4:5060";
        let hits = detect(&doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "f1", "fa", None)),
            dg(1_200_000, B, A, response(200, "OK", 1, "INVITE", "f1", "fa", Some("t1"))),
            dg(1_250_000, A, B, request("ACK", 1, "f1", "fa", Some("t1"))),
            dg(1_400_000, FORK, A, response(200, "OK", 1, "INVITE", "f1", "fa", Some("t2"))),
            dg(30_000_000, A, B, request("BYE", 2, "f1", "fa", Some("t1"))),
            dg(30_100_000, B, A, response(200, "OK", 2, "BYE", "f1", "fa", Some("t1"))),
        ]));
        assert_eq!(hits.len(), 1, "the second fork's dialog went unconfirmed: {hits:?}");
        assert_eq!(hits[0].taker, FORK);
        let Evidence::NoAck { to_tag, .. } = &hits[0].evidence else { panic!("ack") };
        assert_eq!(to_tag, "t2", "same INVITE, different dialog, different obligation");
    }

    /// The corpus shape, and the reason a hop is not a UAC: one Call-ID across
    /// a proxy that does NOT record-route, captured on both of its wires. The
    /// caller ACKs the far UAS DIRECTLY, so the proxy never sees the ACK for a
    /// 2xx it demonstrably relayed. Charging it would charge the one box on the
    /// wire that owes nothing.
    fn through_a_proxy(id: &str, ack: bool) -> Vec<crate::Datagram> {
        let mut out = vec![
            dg(0, A, P, request("INVITE", 1, id, "fa", None)),
            dg(53_000, P, B, request("INVITE", 1, id, "fa", None)),
            dg(7_640_000, B, P, response(200, "OK", 1, "INVITE", id, "fa", Some("tb"))),
            dg(7_659_000, P, A, response(200, "OK", 1, "INVITE", id, "fa", Some("tb"))),
        ];
        if ack {
            out.push(dg(7_661_000, A, B, request("ACK", 1, id, "fa", Some("tb"))));
        }
        out.extend([
            dg(13_747_000, B, A, request("BYE", 9, id, "tb", Some("fa"))),
            dg(13_764_000, A, B, response(200, "OK", 9, "BYE", id, "tb", Some("fa"))),
        ]);
        out
    }

    #[test]
    fn a_hop_that_relayed_the_invite_owes_no_ack() {
        let scanned = scan(&doc_of(through_a_proxy("px", true)));
        assert!(
            scanned.hits.is_empty(),
            "the UAC ACKed direct and the proxy owes nothing: {:?}",
            scanned.hits
        );
        let p = scanned.population["no-ack-to-dialog-creating-2xx"];
        assert_eq!(
            (p.occasions, p.decided),
            (2, 2),
            "one dialog, acknowledged on the leg: both takers' obligations are met"
        );
    }

    /// The RED PROOF for the gate above: the SAME ladder with the ACK removed.
    /// Exactly one endpoint is charged — the UAC that opened the INVITE — and
    /// the proxy that relayed it is still not.
    #[test]
    fn the_uac_behind_a_relaying_hop_is_the_one_charged() {
        let hits = detect(&doc_of(through_a_proxy("py", false)));
        assert_eq!(hits.len(), 1, "one obligation, one holder: {hits:?}");
        assert_eq!(hits[0].emitter, A, "the UAC opened the INVITE");
        assert_eq!(hits[0].taker, P, "it took the 2xx from the hop in front of it");
        assert!(!hits[0].relayed);
    }

    /// The reference platform's own shape, and the one the ruling of
    /// 2026-08-23 names: a B2BUA re-originates the call on a b-leg of its own,
    /// takes the 200 that confirms that dialog through a proxy, and then says
    /// NOTHING — no ACK, no BYE — while the capture goes on recording its a-leg
    /// for another twenty seconds.
    fn re_originated_b_leg(id: &str, ack: bool) -> Vec<crate::Datagram> {
        const SBC: &str = "10.0.0.5:5060";
        let a = format!("a-{id}");
        let mut out = vec![
            // The a-leg the B2BUA takes and answers, ACKed and BYEd normally.
            dg(0, SBC, P, request("INVITE", 145_653, &a, "fa", None)),
            dg(6_000, P, B, request("INVITE", 145_653, &a, "fa", None)),
            dg(1_352_000, P, SBC, response(200, "OK", 145_653, "INVITE", &a, "fa", Some("tb"))),
            dg(1_362_000, SBC, B, request("ACK", 145_653, &a, "fa", Some("tb"))),
            dg(21_563_000, SBC, B, request("BYE", 145_657, &a, "fa", Some("tb"))),
            dg(21_583_000, B, SBC, response(200, "OK", 145_657, "BYE", &a, "fa", Some("tb"))),
            // The b-leg it minted, on its own Call-ID and its own CSeq space.
            dg(2_000, B, P, request_tok("INVITE", 605_367, id, "gb", None, &a)),
            dg(8_000, P, C, request_tok("INVITE", 605_367, id, "gb", None, &a)),
            dg(1_257_000, C, P, response(200, "OK", 605_367, "INVITE", id, "gb", Some("tc"))),
            dg(1_258_000, P, B, response(200, "OK", 605_367, "INVITE", id, "gb", Some("tc"))),
        ];
        if ack {
            out.push(dg(1_262_000, B, C, request_tok("ACK", 605_367, id, "gb", Some("tc"), &a)));
        }
        out
    }

    #[test]
    fn the_b2buas_own_b_leg_2xx_is_charged_to_the_b2bua() {
        let hits = detect(&doc_of(re_originated_b_leg("bleg", false)));
        assert_eq!(hits.len(), 1, "one un-ACKed dialog, on the b-leg: {hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.emitter, B, "the B2BUA opened the b-leg INVITE and took its 200");
        assert_eq!(hit.taker, P, "through the proxy in front of it");
        assert_eq!(hit.cseq, 605_367);
        assert_eq!(
            hit.emitter_role.token(),
            "platform",
            "the box both legs cross is the platform, and the report says which system it is"
        );
        let Evidence::NoAck { window_us, emitter_window_us, bye_after_us, retransmits, .. } =
            &hit.evidence
        else {
            panic!("ack")
        };
        assert_eq!(*window_us, 20_325_000, "the CAPTURE ran on, whatever the b-leg did");
        assert_eq!(
            *emitter_window_us, 20_325_000,
            "and it kept carrying the charged endpoint's own a-leg traffic"
        );
        assert_eq!((*bye_after_us, *retransmits), (None, 0), "the b-leg simply went silent");
    }

    /// The RED PROOF for the shape above: the same two legs with the B2BUA's
    /// b-leg ACK on board, sent DIRECT to the far UAS past the proxy. Nothing
    /// is charged — not the B2BUA, and not the proxy that relayed the 200.
    #[test]
    fn the_same_b_leg_with_its_ack_charges_nobody() {
        let scanned = scan(&doc_of(re_originated_b_leg("bleg-ok", true)));
        assert!(scanned.hits.is_empty(), "the b-leg was confirmed: {:?}", scanned.hits);
    }
}
