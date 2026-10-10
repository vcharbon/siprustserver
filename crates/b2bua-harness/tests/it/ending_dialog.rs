//! A dialog this stack is ending still exists until its BYE takes a final
//! (RFC 3261 §15.1.2), so its peer's requests crossing that BYE are answered
//! in it: a BYE crossing ours takes 200; an INFO, MESSAGE or OPTIONS takes
//! 200 and goes no further, the other party's dialog being over; a re-INVITE,
//! UPDATE or REFER takes 487 Request Terminated (§21.4.25). Once the BYE took
//! its final the dialog is over and a late BYE draws 481
//! (`dialog_gate::a_media_leg_request_no_rule_takes_is_refused_and_its_late_bye_481`).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Dialog as PartyDialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendonly\r\n";

/// A bridged call alice → bob that bob hangs up: the B2BUA's BYE toward
/// alice is on the wire, received and not yet answered — her dialog is
/// ending.
struct Ending {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    /// Alice's side of her dialog with the B2BUA.
    dialog: PartyDialog,
    /// The B2BUA's BYE as alice received it.
    ours: ServerTxn,
}

async fn ending(name: &str) -> Ending {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    uas.dialog().bye().await.expect(200).await;
    let ours = alice.receive("BYE").await;
    Ending { h, alice, bob, b2bua, dialog, ours }
}

impl Ending {
    /// Nothing alice sent reached bob, whose dialog is over.
    async fn assert_nothing_crossed(&self, method: &str) {
        self.h.advance(Duration::from_millis(100)).await;
        assert!(
            self.bob.try_receive_tolerating(method, &[]).await.is_none(),
            "no {method} reached bob"
        );
    }

    /// Alice answers the B2BUA's BYE; the call reaps clean.
    async fn finish(mut self) {
        self.ours.respond(200, "OK").await;
        settle_until(|| self.b2bua.is_reaped()).await;
        self.alice.drain().await;
        self.bob.drain().await;
        self.b2bua.assert_fully_reaped();
        let _ = self.h.finish().await;
    }
}

/// An INFO, a MESSAGE and an OPTIONS crossing the B2BUA's BYE are answered
/// 200 in the dialog that BYE is ending, and none goes further; her own BYE
/// crossing it is answered 200 too.
#[tokio::test(start_paused = true)]
async fn an_ending_dialog_answers_info_message_and_options_200_and_relays_none() {
    let mut s = ending("ending-dialog-absorbs").await;
    for method in [InDialogMethod::Info, InDialogMethod::Message, InDialogMethod::Options] {
        let mut txn = s.dialog.send_request(method).send().await;
        let ok = txn.expect(200).await;
        assert_eq!(ok.to().tag(), Some(s.dialog.remote_tag()), "{method:?}: in her dialog");
        s.assert_nothing_crossed(method.as_str()).await;
    }
    s.dialog.bye().await.expect(200).await;
    s.finish().await;
}

/// A re-INVITE, an UPDATE and a REFER crossing the B2BUA's BYE ask the
/// ending dialog for what it will no longer do: 487 Request Terminated
/// (RFC 3261 §21.4.25), the re-INVITE's ACKed, and none goes further.
#[tokio::test(start_paused = true)]
async fn an_ending_dialog_answers_a_reinvite_an_update_and_a_refer_487() {
    let mut s = ending("ending-dialog-terminates").await;
    s.dialog.reinvite(Some(REOFFER)).await.expect(487).await;
    s.assert_nothing_crossed("INVITE").await;
    s.dialog.send_request(InDialogMethod::Update).with_sdp(REOFFER).send().await.expect(487).await;
    s.assert_nothing_crossed("UPDATE").await;
    let refer_to = format!("<sip:carol@{}>", s.bob.addr());
    s.dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to)
        .send()
        .await
        .expect(487)
        .await;
    s.assert_nothing_crossed("REFER").await;
    s.finish().await;
}
