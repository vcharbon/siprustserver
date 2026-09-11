//! Cross-type integration tests for the fluent agent surface: the §22.2 auth
//! retry seam and the end-to-end two-view contract of [`crate::absorption`].
//! Pure classification tests live next to that module.

use std::sync::Arc;

use sip_message::generators::{InDialogMethod, OutOfDialogMethod};
use sip_message::header::HeaderName;
use sip_message::SipMessage;

use super::{Harness, StepError};
use crate::realcall::auth::{Challenge, ChallengeResponder};

const OFFER: &str = "v=0\r\no=a 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

mod auth_seam {
    //! The deferred-by-design [`ChallengeResponder`] retry plumbing (RFC 3261
    //! §22.2), exercised on the fallible INVITE surface. A FAKE responder (a
    //! static credential) proves the ACK→resend→credential→bumped-CSeq path; a
    //! run with NO responder proves the classification is unchanged (a `401`
    //! stays a `WrongStatus`).

    use super::*;

    /// A static-credential responder: returns a fixed `Authorization` value for
    /// any challenge (the deferred seam's simplest possible implementation — real
    /// digest would hash `challenge.header_value` + `method`/`ruri`). Records what
    /// it was asked so the test can assert the request-line inputs reached it.
    struct FakeResponder {
        credential: String,
        seen: std::sync::Mutex<Vec<(u16, String, String)>>,
    }
    impl ChallengeResponder for FakeResponder {
        fn respond(&self, challenge: &Challenge, method: &str, ruri: &str) -> Option<String> {
            self.seen.lock().unwrap().push((
                challenge.status,
                method.to_string(),
                ruri.to_string(),
            ));
            Some(self.credential.clone())
        }
    }

    /// Direct plumbing: alice INVITEs a UAS that `401`s once (with a
    /// `WWW-Authenticate` challenge) then admits. The retry ACKs the challenge,
    /// adds the responder's `Authorization`, bumps the CSeq, resends, and the call
    /// completes — proving `ClientInvite::ack_and_resend_with_auth` end to end.
    #[tokio::test(start_paused = true)]
    async fn auth_retry_acks_resends_with_credential_and_bumped_cseq() {
        let h = Harness::new("auth-retry-plumbing");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let responder = FakeResponder {
            credential:
                "Digest username=\"alice\", realm=\"sip\", nonce=\"abc\", response=\"deadbeef\""
                    .to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        };

        // Alice's INVITE #1 goes straight to the server.
        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;

        // The server challenges with a 401 + WWW-Authenticate.
        let mut chal = server.try_receive("INVITE").await.unwrap();
        assert_eq!(chal.request().cseq().seq(), 1, "first INVITE is CSeq 1");
        chal.respond(401, "Unauthorized")
            .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"abc\"")
            .try_send()
            .await
            .unwrap();

        // Alice sees the 401 (raw, un-asserted) and drives the retry.
        let resp = call.try_recv_response().await.unwrap();
        assert_eq!(resp.status(), 401);
        let resent = call.ack_and_resend_with_auth(&resp, &responder).await.unwrap();
        assert!(resent, "responder returned a credential → a resend happened");

        // The responder saw the challenge status + the request-line inputs.
        {
            let seen = responder.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, 401);
            assert_eq!(seen[0].1, "INVITE");
            assert!(seen[0].2.starts_with("sip:server@"), "ruri passed through: {}", seen[0].2);
        }

        // The server first sees the ACK for the 401 (RFC 3261 §17.1.1.3)…
        let ack = server.try_receive("ACK").await.unwrap();
        assert_eq!(ack.request().cseq().seq(), 1, "the non-2xx ACK reuses the INVITE CSeq");

        // …then the resent, authenticated INVITE #2: CSeq bumped, Authorization added.
        let mut admit = server.try_receive("INVITE").await.unwrap();
        assert_eq!(admit.request().cseq().seq(), 2, "the retried INVITE bumps the CSeq (§22.2)");
        assert!(
            admit
                .request()
                .raw(HeaderName::Authorization)
                .next()
                .is_some_and(|v| v.starts_with("Digest ")),
            "the retried INVITE carries the responder's Authorization",
        );

        // The server admits; alice completes the call.
        admit.respond(180, "Ringing").try_send().await.unwrap();
        call.try_expect(180).await.unwrap();
        admit.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();
        call.try_expect(200).await.unwrap();
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();

        // Teardown.
        let mut bye = dialog.bye().await;
        server.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        bye.try_expect(200).await.unwrap();

        let _ = h.finish().await;
    }

    /// `try_expect_final` absorbs (and collects) any interleaved provisionals
    /// — the SIPp-`optional` semantics — instead of erroring on a 1xx the body
    /// did not hard-code, and still learns the dialog state the final confirms
    /// (the ACK/BYE route correctly after).
    #[tokio::test(start_paused = true)]
    async fn try_expect_final_absorbs_and_collects_provisionals() {
        let h = Harness::new("try-expect-final");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.try_receive("INVITE").await.unwrap();
        // A relay-timing-dependent provisional mix: 180 then 183, then the 200.
        uas.respond(180, "Ringing").try_send().await.unwrap();
        uas.respond(183, "Session Progress").try_send().await.unwrap();
        uas.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();

        let (answer, provisionals) = call.try_expect_final(200).await.unwrap();
        assert_eq!(answer.status(), 200);
        assert_eq!(
            provisionals.iter().map(|p| p.status()).collect::<Vec<_>>(),
            vec![180, 183],
            "every absorbed 1xx is collected, in arrival order"
        );

        // The learned dialog state routes the ACK + teardown correctly.
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();
        let mut bye = dialog.bye().await;
        server.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        bye.try_expect(200).await.unwrap();
        let _ = h.finish().await;
    }

    /// `try_receive_tolerating_blocking` waits for the sentinel method,
    /// 200-OKs the tolerated traffic in between, and RETURNS the absorbed
    /// requests so the body can assert them — where a blind drain would let a
    /// lost sentinel become silent success (here it is a `Timeout` error,
    /// asserted first on an idle socket).
    #[tokio::test(start_paused = true)]
    async fn try_receive_tolerating_blocking_collects_absorbed_and_times_out() {
        let h = Harness::new("blocking-tolerant-receive");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        // Establish A↔server.
        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.try_receive("INVITE").await.unwrap();
        uas.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();
        call.try_expect(200).await.unwrap();
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();

        // A lost sentinel is a DETECTABLE failure: nothing is in flight, so the
        // blocking receive times out instead of silently succeeding.
        match server.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await {
            Err(StepError::Timeout { .. }) => {}
            Err(e) => panic!("an absent sentinel must surface as Timeout, got {e}"),
            Ok((txn, _)) => panic!(
                "an absent sentinel must surface as Timeout, got a {} request",
                txn.request().method()
            ),
        }

        // NOTIFY(s) then the BYE — the nondeterministic-count release pattern
        // (the ct_refer shape). The primitive 200s the NOTIFY, returns on the
        // BYE, and hands the absorbed NOTIFY back for assertion.
        let mut notify = dialog.send_request(InDialogMethod::Notify).try_send().await.unwrap();
        let mut bye = dialog.bye().await;

        let (mut bye_txn, absorbed) =
            server.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await.unwrap();
        assert_eq!(
            absorbed.iter().map(|r| r.method().to_string()).collect::<Vec<_>>(),
            vec!["NOTIFY".to_string()],
            "the absorbed traffic is returned, assertable"
        );
        bye_txn.respond(200, "OK").try_send().await.unwrap();

        // Alice's side settles: the primitive's 200 (NOTIFY) and her BYE 200.
        notify.try_expect(200).await.unwrap();
        bye.try_expect(200).await.unwrap();
        let _ = h.finish().await;
    }

    /// The out-of-dialog twin (`OutOfDialogRequest::try_send_authed`, the
    /// REGISTER seam): a server `401`s the first OPTIONS then `200`s the
    /// credentialed resend. No ACK (a non-INVITE final needs none, §17.1.2.2);
    /// the resend bumps the CSeq and carries the responder's `Authorization`.
    #[tokio::test(start_paused = true)]
    async fn out_of_dialog_try_send_authed_retries_once() {
        let h = Harness::new("auth-ood-retry");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let responder: Arc<dyn ChallengeResponder> = Arc::new(FakeResponder {
            credential: "Digest username=\"alice\", realm=\"sip\", response=\"y\"".to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        });

        let server_rx = server.clone();
        let srv = tokio::spawn(async move {
            let server = server_rx;
            // First OPTIONS → 401.
            let mut c = server.try_receive("OPTIONS").await.unwrap();
            assert_eq!(c.request().cseq().seq(), 1);
            assert!(c.request().raw(HeaderName::Authorization).next().is_none());
            c.respond(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n\"")
                .try_send()
                .await
                .unwrap();
            // Credentialed resend → 200. CSeq bumped, Authorization present.
            let mut c2 = server.try_receive("OPTIONS").await.unwrap();
            assert_eq!(c2.request().cseq().seq(), 2, "the authed resend bumps the CSeq");
            assert!(
                c2.request().raw(HeaderName::Authorization).next().is_some(),
                "the resend carries the Authorization",
            );
            c2.respond(200, "OK").try_send().await.unwrap();
        });

        let resp = alice
            .request(OutOfDialogMethod::Options, &server)
            .try_send_authed(Some(responder.as_ref()), 200)
            .await
            .expect("the authenticated OPTIONS resolves to 200");
        assert_eq!(resp.status(), 200);

        srv.await.unwrap();
        let _ = h.finish().await;
    }

    /// The out-of-dialog path with NO responder: the `401` surfaces as a plain
    /// `WrongStatus` (no retry), unchanged from `try_send` + `try_expect`.
    #[tokio::test(start_paused = true)]
    async fn out_of_dialog_without_responder_surfaces_401() {
        let h = Harness::new("auth-ood-no-responder");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let server_rx = server.clone();
        let srv = tokio::spawn(async move {
            let mut c = server_rx.try_receive("OPTIONS").await.unwrap();
            c.respond(401, "Unauthorized").try_send().await.unwrap();
        });

        match alice.request(OutOfDialogMethod::Options, &server).try_send_authed(None, 200).await {
            Err(StepError::WrongStatus { got: 401, expected: 200, .. }) => {}
            Err(other) => panic!("expected WrongStatus 200/401, got {other:?}"),
            Ok(r) => panic!("expected a 401 deviation, got {}", r.status()),
        }

        srv.await.unwrap();
        let _ = h.finish().await;
    }
}

mod absorption_end_to_end {
    //! The §17.2 once-and-only-once receive view, end to end: a Timer-A style
    //! duplicate never surfaces (no `receive_absorbing` lists needed);
    //! `drop_to_raw_wire()` restores the raw surface.

    use super::*;

    /// The headline contract: a Timer-A style INVITE retransmission never
    /// surfaces, so the callee needs NO `receive_absorbing` list — the exact
    /// pattern that used to require one (silent-callee duplicates queued ahead
    /// of the ACK would make `receive("ACK")` fail with
    /// "expected a ACK request, got INVITE").
    #[tokio::test(start_paused = true)]
    async fn invite_retransmits_absorbed_without_lists() {
        let h = Harness::new("txn-view-invite-retransmit");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        // Byte-identical Timer-A duplicates, straight from the original request.
        let dup = SipMessage::Request(call.original_invite.clone());
        alice.send(&dup, call.wire_dst).await;
        alice.send(&dup, call.wire_dst).await;

        let mut uas = server.receive("INVITE").await;
        uas.respond(180, "Ringing").send().await;
        call.expect(180).await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        // The two duplicates are queued ahead of the ACK — absorbed below the API.
        server.receive("ACK").await;

        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;
        let _ = h.finish().await;
    }

    /// A byte-identical 2xx repeat (Timer-G style) is absorbed — and can no
    /// longer be mis-taken for the answer to a LATER transaction (a status-only
    /// `expect(200)` would otherwise return the duplicate 200-INVITE as the
    /// BYE's answer).
    #[tokio::test(start_paused = true)]
    async fn duplicate_final_not_mistaken_for_later_answer() {
        let h = Harness::new("txn-view-final-dedup");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        // The 2xx repeat: sticky To-tag + same SDP ⇒ byte-identical.
        uas.respond(200, "OK").with_sdp(OFFER).send().await;

        let mut dialog = call.ack().await;
        server.receive("ACK").await;
        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        let resp = bye.expect(200).await;
        assert_eq!(
            resp.cseq().method().to_string(),
            "BYE",
            "the duplicate 200-INVITE was absorbed, not returned as the BYE answer"
        );
        let _ = h.finish().await;
    }

    /// `drop_to_raw_wire()` restores the raw surface: the duplicate SURFACES
    /// again and the `receive_absorbing` idiom is once more the caller's job —
    /// the sanctioned escape hatch for a body that must pull each repeat.
    #[tokio::test(start_paused = true)]
    async fn wire_view_restores_raw_duplicates() {
        let h = Harness::new("txn-view-wire-optout");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;
        server.drop_to_raw_wire();

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        alice.send(&SipMessage::Request(call.original_invite.clone()), call.wire_dst).await;

        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        // The duplicate INVITE is still queued and SURFACES — absorb it the old way.
        server.receive_absorbing("ACK", &["INVITE"]).await;

        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;
        let _ = h.finish().await;
    }
}

mod two_view_ladders {
    //! One ladder per row of the classification table in [`crate::absorption`]
    //! (issue 22), each asserting BOTH views of the same stream. Retransmission
    //! is driven explicitly on the paused clock — the harness UA runs no Timer
    //! A/E of its own, so the test IS the timer and never leaps two deadlines.

    use super::*;
    use crate::absorption::SeenBy;

    /// T1 and its first doubling (RFC 3261 §17.1.1.2): the interval this ladder
    /// re-sends on.
    const T1: std::time::Duration = std::time::Duration::from_millis(500);
    const T2: std::time::Duration = std::time::Duration::from_millis(1000);

    fn count(view: &[crate::absorption::WireEntry], starts_with: &str) -> usize {
        view.iter().filter(|e| e.start_line().starts_with(starts_with)).count()
    }

    /// Row 1: a retransmitted INVITE is on the wire three times and reaches the
    /// transaction user once.
    #[tokio::test(start_paused = true)]
    async fn retransmitted_invite_is_wire_only_and_the_tu_sees_one() {
        let h = Harness::new("two-view-invite-ladder");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let dup = SipMessage::Request(call.original_invite.clone());
        // The Timer-A ladder, one deadline at a time.
        h.advance(T1).await;
        alice.send(&dup, call.wire_dst).await;
        h.advance(T2).await;
        alice.send(&dup, call.wire_dst).await;

        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        server.receive("ACK").await;

        assert_eq!(count(&server.wire_view(), "INVITE"), 3, "{:#?}", server.wire_view());
        assert_eq!(count(&server.tu_view(), "INVITE"), 1, "the TU saw one INVITE");
        assert!(
            server.wire_view().iter().filter(|e| e.is_repeat()).count() == 2,
            "both repeats are tagged as repeats",
        );
        assert!(
            server
                .wire_view()
                .iter()
                .filter(|e| e.is_repeat())
                .all(|e| e.seen_by() == SeenBy::WireOnly),
            "an absorbed INVITE repeat is wire-only",
        );

        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;
        let _ = h.finish().await;
    }

    /// Row 2, both cells: a retransmitted non-2xx final is wire-only at the
    /// caller, and the hop ACK it elicits is wire-only at the callee — the
    /// INVITE server transaction owns it, not the TU.
    #[tokio::test(start_paused = true)]
    async fn a_retransmitted_non_2xx_final_and_its_hop_ack_are_wire_only() {
        let h = Harness::new("two-view-non-2xx-ladder");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).send().await;
        let mut uas = server.receive("INVITE").await;
        // The 486 arms the §17.1.1.3 obligation; alice's client txn auto-ACKs.
        uas.respond(486, "Busy Here").send().await;
        call.expect(486).await;
        uas.expect_ack().await;

        // Timer G: the identical 486 again, one deadline later.
        h.advance(T1).await;
        uas.respond(486, "Busy Here").send().await;
        h.advance(T1).await;
        // Nothing will ever PULL the repeat — the wire view still owes it.
        alice.sight_queued().await;

        let alice_wire = alice.wire_view();
        assert_eq!(count(&alice_wire, "SIP/2.0 486"), 2, "{alice_wire:#?}");
        assert_eq!(count(&alice.tu_view(), "SIP/2.0 486"), 1, "the TU saw one 486");

        let server_wire = server.wire_view();
        assert_eq!(count(&server_wire, "ACK"), 1, "one hop ACK on the wire");
        assert_eq!(count(&server.tu_view(), "ACK"), 0, "the hop ACK never reaches the TU");
        let _ = h.finish().await;
    }

    /// Row 3 + row 4, the trap: a retransmitted 2xx stays in BOTH views (the
    /// TU re-sends), and the fresh ACK it elicits is in both views at the
    /// callee — the UAC core, not the transaction layer, sends every one.
    #[tokio::test(start_paused = true)]
    async fn a_retransmitted_2xx_and_the_ack_it_elicits_are_in_both_views() {
        let h = Harness::new("two-view-2xx-ladder");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        server.receive("ACK").await;

        // §13.3.1.4: the TU retransmits the 2xx until it is ACKed again.
        h.advance(T1).await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        h.advance(T1).await;
        // Alice's core sees it and re-ACKs; the callee pulls that ACK.
        let mut bye = dialog.bye().await;
        server.receive_tolerating("BYE", &[]).await.respond(200, "OK").send().await;
        bye.expect(200).await;
        h.advance(T1).await;
        alice.sight_queued().await;
        server.sight_queued().await;

        let alice_wire = alice.wire_view();
        let repeats: Vec<_> =
            alice_wire.iter().filter(|e| e.start_line().starts_with("SIP/2.0 200")).collect();
        assert_eq!(repeats.len(), 3, "200-to-INVITE twice + the BYE's: {alice_wire:#?}");
        assert_eq!(
            count(&alice.tu_view(), "SIP/2.0 200"),
            3,
            "a 2xx retransmission is end-to-end — the TU view keeps it",
        );
        assert!(
            alice_wire.iter().filter(|e| e.is_repeat()).all(|e| e.seen_by() == SeenBy::Both),
            "the ONLY repeat here is the 2xx, and it is in both views",
        );

        let server_wire = server.wire_view();
        assert_eq!(count(&server_wire, "ACK"), 2, "each 2xx drew its own ACK: {server_wire:#?}");
        assert_eq!(count(&server.tu_view(), "ACK"), 2, "and the TU sees every one");
        let _ = h.finish().await;
    }

    /// Row 4 on its own, the normal path: one 2xx, one ACK, and both views
    /// agree on the whole exchange because nothing was absorbed.
    #[tokio::test(start_paused = true)]
    async fn the_ack_to_a_2xx_is_in_both_views_on_the_normal_path() {
        let h = Harness::new("two-view-normal-ack");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.receive("INVITE").await;
        uas.respond(180, "Ringing").send().await;
        call.expect(180).await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        server.receive("ACK").await;
        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;

        let server_wire = server.wire_view();
        assert_eq!(count(&server_wire, "ACK"), 1);
        assert_eq!(count(&server.tu_view(), "ACK"), 1);
        assert!(server_wire.iter().all(|e| e.seen_by() == SeenBy::Both));
        assert_eq!(
            server_wire.len(),
            server.tu_view().len(),
            "nothing was absorbed, so the two views are the same stream",
        );
        assert_eq!(alice.wire_view().len(), alice.tu_view().len());
        let _ = h.finish().await;
    }
}
