//! Cross-type integration tests for the fluent agent surface: the §22.2 auth
//! retry seam and the end-to-end §17.2 receive-view contract. Pure
//! [`TxnView`](super::txn_view::TxnView) verdict tests live next to the type.

use std::sync::Arc;

use sip_message::generators::{InDialogMethod, OutOfDialogMethod};
use sip_message::message_helpers::get_header;
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
            credential: "Digest username=\"alice\", realm=\"sip\", nonce=\"abc\", response=\"deadbeef\""
                .to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        };

        // Alice's INVITE #1 goes straight to the server.
        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;

        // The server challenges with a 401 + WWW-Authenticate.
        let mut chal = server.try_receive("INVITE").await.unwrap();
        assert_eq!(chal.request().cseq.seq, 1, "first INVITE is CSeq 1");
        chal.respond(401, "Unauthorized")
            .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"abc\"")
            .try_send()
            .await
            .unwrap();

        // Alice sees the 401 (raw, un-asserted) and drives the retry.
        let resp = call.try_recv_response().await.unwrap();
        assert_eq!(resp.status, 401);
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
        assert_eq!(ack.request().cseq.seq, 1, "the non-2xx ACK reuses the INVITE CSeq");

        // …then the resent, authenticated INVITE #2: CSeq bumped, Authorization added.
        let mut admit = server.try_receive("INVITE").await.unwrap();
        assert_eq!(admit.request().cseq.seq, 2, "the retried INVITE bumps the CSeq (§22.2)");
        assert!(
            get_header(&admit.request().headers, "authorization")
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
        assert_eq!(answer.status, 200);
        assert_eq!(
            provisionals.iter().map(|p| p.status).collect::<Vec<_>>(),
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
    /// requests so the body can assert them — where `quiesce` blind-drains and
    /// a lost sentinel becomes silent success (here it is a `Timeout` error,
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
                txn.request().method
            ),
        }

        // NOTIFY(s) then the BYE — the nondeterministic-count release pattern
        // (the ct_refer shape). The primitive 200s the NOTIFY, returns on the
        // BYE, and hands the absorbed NOTIFY back for assertion.
        let mut notify =
            dialog.send_request(InDialogMethod::Notify).try_send().await.unwrap();
        let mut bye = dialog.bye().await;

        let (mut bye_txn, absorbed) =
            server.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await.unwrap();
        assert_eq!(
            absorbed.iter().map(|r| r.method.to_string()).collect::<Vec<_>>(),
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
            assert_eq!(c.request().cseq.seq, 1);
            assert!(get_header(&c.request().headers, "authorization").is_none());
            c.respond(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n\"")
                .try_send()
                .await
                .unwrap();
            // Credentialed resend → 200. CSeq bumped, Authorization present.
            let mut c2 = server.try_receive("OPTIONS").await.unwrap();
            assert_eq!(c2.request().cseq.seq, 2, "the authed resend bumps the CSeq");
            assert!(
                get_header(&c2.request().headers, "authorization").is_some(),
                "the resend carries the Authorization",
            );
            c2.respond(200, "OK").try_send().await.unwrap();
        });

        let resp = alice
            .request(OutOfDialogMethod::Options, &server)
            .try_send_authed(Some(responder.as_ref()), 200)
            .await
            .expect("the authenticated OPTIONS resolves to 200");
        assert_eq!(resp.status, 200);

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

        match alice
            .request(OutOfDialogMethod::Options, &server)
            .try_send_authed(None, 200)
            .await
        {
            Err(StepError::WrongStatus { got: 401, expected: 200, .. }) => {}
            Err(other) => panic!("expected WrongStatus 200/401, got {other:?}"),
            Ok(r) => panic!("expected a 401 deviation, got {}", r.status),
        }

        srv.await.unwrap();
        let _ = h.finish().await;
    }
}

mod txn_view_end_to_end {
    //! The §17.2 once-and-only-once receive view, end to end: a Timer-A style
    //! duplicate never surfaces (no `receive_absorbing` lists needed);
    //! `wire_view()` restores the raw surface.

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
            resp.cseq.method.to_string(),
            "BYE",
            "the duplicate 200-INVITE was absorbed, not returned as the BYE answer"
        );
        let _ = h.finish().await;
    }

    /// `wire_view()` restores the raw surface: the duplicate SURFACES again and
    /// the `receive_absorbing` idiom is once more the caller's job — the
    /// sanctioned escape hatch for tests whose subject is retransmission.
    #[tokio::test(start_paused = true)]
    async fn wire_view_restores_raw_duplicates() {
        let h = Harness::new("txn-view-wire-optout");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;
        server.wire_view();

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
