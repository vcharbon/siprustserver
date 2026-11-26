//! **A caller's request pending on the early dialog her CANCEL ends draws 487**
//! (RFC 3261 §9.2, §12.3, §15.1.2). The callee rings reliably and the caller
//! PRACKs (RFC 3262 §3), then sends a request on that early dialog and CANCELs
//! before its final returns. This stack answers the INVITE 487, which ends the
//! early dialog; the pending request is still owed its one final, and it is
//! 487. The callee answers that request 200 before he reads the CANCEL this
//! stack sent him; his 200 is absorbed: never relayed to a dialog that is gone.
//!
//! The guards: a caller BYE on the early dialog ends it the same way, and her
//! pending request draws the same 487; a PRACK pending at the CANCEL draws the
//! 200 the method table owes a PRACK at a leg's end (RFC 3262 §3).
//!
//! ```text
//!   alice            b2bua             bob
//!     INVITE →           INVITE →
//!                    ←   183 (rel) ; PRACK ↔ 200
//!     UPDATE →           UPDATE →
//!     CANCEL →
//!     ← 200 ; ← 487 (INVITE) ; ← 487 (UPDATE)
//!                        CANCEL →
//!                    ←   200 (UPDATE)        [absorbed]
//!                    ←   200 (CANCEL) ; 487 (INVITE)
//!                        ACK →
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use call::TerminationCause;
use scenario_harness::{Harness, Party, RunReport};
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Past every hop of the teardown and short of a retransmission (T1).
const DRAINED: Duration = Duration::from_millis(300);

/// How the caller ends her early dialog.
#[derive(Clone, Copy)]
enum Ending {
    /// She CANCELs her INVITE (§9.1).
    Cancel,
    /// She BYEs the early dialog (§15).
    Bye,
}

/// The finals `to` received to its requests of `method`, in arrival order.
fn finals_of(report: &RunReport, to: SocketAddr, method: &str) -> Vec<u16> {
    report
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match CustomParser::built().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() >= 200 && r.cseq().method().as_str() == method =>
            {
                Some(r.status())
            }
            _ => None,
        })
        .collect()
}

/// alice's `method` on her early dialog is pending at bob when she ends that
/// dialog by `ending`: it draws `owed` from this stack before bob answers it,
/// bob's 200 to it is absorbed, and the call ends on her release with one
/// final per transaction.
/// A pending PRACK is the PRACK of the 183; any other request follows it.
async fn pending_request_draws(name: &str, method: InDialogMethod, ending: Ending, owed: u16) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;
    let alice_addr = alice.addr();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    let shown = call.expect(183).await;
    let a_tag = shown.to().tag().expect("the 183 opens an early dialog").to_string();
    let rseq = shown.header::<RSeq>().expect("an RSeq").expect("a readable RSeq");
    let rack = format!("{} {} INVITE", rseq.value(), Party::Caller.first_cseq());
    let is_prack = method == InDialogMethod::Prack;
    if !is_prack {
        let mut prack = call
            .send_request(InDialogMethod::Prack)
            .with_to_tag(&a_tag)
            .with_rack(&rack)
            .send()
            .await;
        bob.receive("PRACK").await.respond(200, "OK").await;
        prack.expect(200).await;
    }

    let request = call.send_request(method).with_to_tag(&a_tag);
    let request = if is_prack { request.with_rack(&rack) } else { request };
    let mut pending = request.send().await;
    let mut at_bob = bob.receive(method.as_str()).await;

    match ending {
        Ending::Cancel => {
            let mut cxl = call.cancel().await;
            cxl.expect(200).await;
        }
        Ending::Bye => {
            let mut bye = call.send_request(InDialogMethod::Bye).with_to_tag(&a_tag).send().await;
            bye.expect(200).await;
        }
    }
    call.expect(487).await;
    pending.expect(owed).await;
    at_bob.respond(200, "OK").await;

    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(DRAINED).await;
    let late = alice.take_queued().await;
    assert!(late.is_none(), "bob's 200 is absorbed, alice is sent nothing more: {late:?}");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let cause = cdrs[0].termination.as_ref().map(|t| t.cause);
    let released = match ending {
        Ending::Cancel => TerminationCause::RemoteCancel,
        Ending::Bye => TerminationCause::RemoteBye,
    };
    assert_eq!(cause, Some(released), "the caller's release ended the call");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice_addr), [487], "one INVITE final");
    let finals = finals_of(&report, alice_addr, method.as_str());
    assert_eq!(finals, [owed], "one final to the pending {}", method.as_str());
}

#[tokio::test(start_paused = true)]
async fn an_update_pending_at_the_cancel_draws_487() {
    let name = "b2bua-update-pending-at-cancel";
    pending_request_draws(name, InDialogMethod::Update, Ending::Cancel, 487).await;
}

#[tokio::test(start_paused = true)]
async fn an_info_pending_at_the_cancel_draws_487() {
    let name = "b2bua-info-pending-at-cancel";
    pending_request_draws(name, InDialogMethod::Info, Ending::Cancel, 487).await;
}

#[tokio::test(start_paused = true)]
async fn an_update_pending_at_an_early_bye_draws_487() {
    let name = "b2bua-update-pending-at-early-bye";
    pending_request_draws(name, InDialogMethod::Update, Ending::Bye, 487).await;
}

#[tokio::test(start_paused = true)]
async fn a_prack_pending_at_the_cancel_draws_200() {
    let name = "b2bua-prack-pending-at-cancel";
    pending_request_draws(name, InDialogMethod::Prack, Ending::Cancel, 200).await;
}
