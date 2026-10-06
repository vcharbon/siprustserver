//! `Harness::settle_before_finish`: a fixture's settle future is awaited by
//! `finish` and `finish_collecting`, in registration order, so work that trails
//! the scenario's last message is done when either returns.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use scenario_harness::Harness;

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// A complete INVITE/180/200/ACK/BYE/200 dialog on `h`.
async fn call(h: &Harness) {
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut call = alice.invite(&bob).with_sdp(SDP_OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(SDP_ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// Registers two settles that finish after a delay and log their order.
fn register(h: &Harness) -> Rc<RefCell<Vec<&'static str>>> {
    let log = Rc::new(RefCell::new(Vec::new()));
    for (name, ms) in [("first", 300), ("second", 10)] {
        let log = log.clone();
        h.settle_before_finish(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            log.borrow_mut().push(name);
        });
    }
    log
}

#[tokio::test(start_paused = true)]
async fn finish_awaits_every_settle_in_registration_order() {
    let h = Harness::new("finish-settles");
    call(&h).await;
    let log = register(&h);
    assert!(log.borrow().is_empty(), "a settle runs at finish, not at registration");
    let _ = h.finish().await;
    assert_eq!(*log.borrow(), ["first", "second"]);
}

#[tokio::test(start_paused = true)]
async fn finish_collecting_awaits_every_settle() {
    let h = Harness::new("finish-collecting-settles");
    call(&h).await;
    let log = register(&h);
    let (_report, gating) = h.finish_collecting().await;
    assert!(gating.is_empty(), "a compliant dialog gates nothing: {gating:?}");
    assert_eq!(*log.borrow(), ["first", "second"]);
}
