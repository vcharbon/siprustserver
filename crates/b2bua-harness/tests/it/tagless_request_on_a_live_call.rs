//! A non-INVITE request with no To-tag names no dialog (RFC 3261 §12.2.1.1):
//! on a live call it is answered 481 (§12.2.2), never relayed, and the call
//! goes on. An INFO in particular rides only an invite dialog usage (RFC 6086
//! §4.2.1). The caller here is non-compliant by design: she sends an INFO
//! before any early dialog reached her, while the callee rings on one early
//! dialog or on two forks.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, ClientInvite, Harness, WaiverScope};
use sip_message::generators::response::is_fallback_to_tag;
use sip_message::generators::InDialogMethod;

/// Why alice's INFO carries no To-tag: her non-compliance is the corner under
/// test, which the audit reads as an in-dialog request lacking the dialog's
/// remote tag.
const TAGLESS_INFO: &str = "alice's INFO before any early dialog reached her carries no To-tag \
                            (RFC 3261 §12.2.1.1), the request under test";

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The B2BUA routing every call to bob, and its two parties; the audit's
/// reading of alice's tagless INFO, the request under test, is waived.
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
}

impl Scene {
    async fn new(name: &str) -> Scene {
        let h = Harness::with_transit_delay(name, 1);
        let alice = h.agent_auto("alice").await;
        let bob = h.agent_auto("bob").await;
        h.waive(
            WaiverScope::rule("in-dialog-to-tag", TAGLESS_INFO).on_party("alice").on_method("INFO"),
        );
        let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
            .start(&h, "b2bua", &h.next_addr().to_string())
            .await;
        Scene { h, alice, bob, b2bua }
    }

    /// alice's INVITE to bob, its Request-URI naming the B2BUA: a request she
    /// sends before any provisional named a remote target goes there too
    /// (RFC 3261 §12.2.1.1).
    async fn invite(&self) -> ClientInvite {
        self.alice
            .invite(&self.bob)
            .ruri(format!("sip:bob@{}", self.b2bua.addr))
            .with_sdp(OFFER)
            .through(self.b2bua.addr)
            .send()
            .await
    }

    /// alice sends an INFO with no To-tag on `call`: it draws a 481 under a
    /// To-tag minted for it (RFC 3261 §8.2.6.2, §19.3), and bob receives
    /// nothing.
    async fn tagless_info_is_refused(&self, call: &mut ClientInvite) {
        let mut info = call.send_request(InDialogMethod::Info).send().await;
        let refusal = info.expect(481).await;
        let tag = refusal.to().tag().expect("the 481 carries a To-tag");
        assert!(!is_fallback_to_tag(tag), "the 481's To-tag is minted, not the fallback: {tag}");
        self.h.advance(Duration::from_millis(100)).await;
        assert!(self.bob.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");
    }

    async fn finish(self) {
        settle_until(|| self.b2bua.is_reaped()).await;
        self.alice.drain().await;
        self.bob.drain().await;
        self.b2bua.assert_fully_reaped();
        let _ = self.h.finish().await;
    }
}

/// The callee rings on one early dialog; the caller, shown none yet, sends
/// an INFO with no To-tag. It draws 481 and never reaches the callee; the
/// callee's answer then completes the call, which alice ends with a BYE.
#[tokio::test(start_paused = true)]
async fn a_tagless_info_while_one_early_dialog_rings_is_refused_481() {
    let s = Scene::new("tagless-info-one-early-dialog").await;
    let mut call = s.invite().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    s.h.advance(Duration::from_millis(10)).await;

    s.tagless_info_is_refused(&mut call).await;

    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// The callee rings on two forks; the caller, shown neither yet, sends an
/// INFO with no To-tag. It draws 481, not a retry-later 500, and never
/// reaches either fork; the caller then CANCELs the call.
#[tokio::test(start_paused = true)]
async fn a_tagless_info_while_two_forks_ring_is_refused_481() {
    let s = Scene::new("tagless-info-two-forks").await;
    let mut call = s.invite().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    s.h.advance(Duration::from_millis(10)).await;

    s.tagless_info_is_refused(&mut call).await;

    call.expect(180).await;
    call.expect(180).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    s.finish().await;
}
