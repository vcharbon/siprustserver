//! RFC 3261 §12.2.2: a request matching a dialog is first held to the rules
//! for requests outside a dialog (§8.2), and only then to its dialog's CSeq
//! order; and a request §8.2 rejects performs no state change. So a request
//! requiring an option tag this stack does not understand draws 420
//! (§8.2.2.3) even when a later request of its sender overtook it, and its
//! refusal leaves the dialog's remote CSeq where it was: a request numbered
//! below it but above the last one taken is still in order.
//!
//! The fabric holds the caller's first INFO in flight, so her second, sent
//! after it, reaches the stack first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::{Dialog, Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::SendDelay;

/// An option tag no implementation defines.
const UNKNOWN: &str = "x-nonexistent";

/// How much later than the transit the first INFO arrives: past the second
/// INFO's arrival, short of the first's retransmission (RFC 3261 §17.1.2.2,
/// T1).
const HELD: Duration = Duration::from_millis(150);

/// Past a request's two hops through the stack to the peer and short of its
/// first retransmission.
const RELAY_WINDOW: Duration = Duration::from_millis(300);

/// The fabric's hold on the first INFO put on it, [`HELD`]; nothing else is
/// held.
fn the_first_info_held() -> SendDelay {
    let held = AtomicBool::new(false);
    Arc::new(move |_src, _dst, datagram| {
        let first = datagram.starts_with(b"INFO ") && !held.swap(true, Ordering::Relaxed);
        first.then_some(HELD)
    })
}

/// A confirmed call alice ↔ b2bua ↔ bob over a fabric that holds the first
/// INFO, and alice's dialog.
async fn confirmed(name: &str) -> (B2buaScene, Dialog) {
    let h = Harness::with_transit_delay_and_send_delay(
        name,
        SIMULATED_TRANSIT_DELAY_MS,
        the_first_info_held(),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;
    let s = B2buaScene { h, alice, bob, b2bua };
    let dialog = s.establish().await;
    (s, dialog)
}

/// alice hangs up; the call ends with its one CDR, fully reaped.
async fn hang_up(s: B2buaScene, mut alice: Dialog) {
    s.hangup(&mut alice).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}

/// alice's INFO requiring the unknown tag is overtaken by her next, plain one,
/// which reaches bob and is answered: her dialog's last CSeq is now above the
/// overtaken INFO's. That INFO still draws 420, not the out-of-order 500, and
/// bob is sent nothing of it.
#[tokio::test(start_paused = true)]
async fn an_overtaken_request_requiring_an_unknown_tag_is_refused_420() {
    let (s, mut alice) = confirmed("overtaken-require-420").await;
    let mut first =
        alice.send_request(InDialogMethod::Info).with_header("Require", UNKNOWN).send().await;
    let mut second = alice.send_request(InDialogMethod::Info).send().await;
    s.bob.receive("INFO").await.respond(200, "OK").await;
    second.expect(200).await;
    let overtaken = first.try_expect(420).await;
    assert!(overtaken.is_ok(), "the overtaken INFO is refused 420, not {overtaken:?}");
    s.h.advance(RELAY_WINDOW).await;
    let relayed = s.bob.take_queued().await;
    assert!(relayed.is_none(), "nothing of the refused INFO reaches bob, got {relayed:?}");
    hang_up(s, alice).await;
}

/// alice's plain INFO is overtaken by her next one, which requires the unknown
/// tag and is refused 420. The refusal left her dialog's remote CSeq as it
/// was, so the plain INFO arriving after it is in order: it reaches bob and
/// his 200 reaches her.
#[tokio::test(start_paused = true)]
async fn a_request_refused_420_leaves_the_order_to_the_request_it_overtook() {
    let (s, mut alice) = confirmed("overtaken-by-a-420").await;
    let mut first = alice.send_request(InDialogMethod::Info).send().await;
    let mut second =
        alice.send_request(InDialogMethod::Info).with_header("Require", UNKNOWN).send().await;
    second.expect(420).await;
    s.bob.receive("INFO").await.respond(200, "OK").await;
    first.expect(200).await;
    hang_up(s, alice).await;
}
