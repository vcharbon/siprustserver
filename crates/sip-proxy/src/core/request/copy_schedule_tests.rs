//! ADR-0063 §1 at the proxy: a copy of a request this hop answered itself
//! draws that answer again for the copies its sender's ladder re-sends, at any
//! pace, and a later one only T1 after the last answered one. The memo of a
//! refused request (503) and its CANCEL's 200 follow it, and so do the
//! preflight rejects (483, 482, 420) through that memo. Each test plays a
//! sender re-sending far faster than its ladder.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use sip_message::SipMessage;

use crate::core::dual_face_tests::{
    caller_invite, caller_src, dual_face_core, dual_face_core_routing, parse_msg, Fixture,
    EXT_PORT, EXT_VIP,
};
use crate::observability::metrics::Face;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
use crate::ProxyAddr;

/// The copies of an INVITE a compliant client re-sends on Timer A (RFC 3261
/// §17.1.1.2, T1 = 500 ms) before Timer B = 64·T1: at 0.5, 1.5, 3.5, 7.5,
/// 15.5 and 31.5 s.
const INVITE_RESENDS: usize = 6;

/// The copies of a non-INVITE request, a CANCEL included, a compliant client
/// re-sends on Timer E (§17.1.2.2, T2 = 4 s) before Timer F = 64·T1: at 0.5,
/// 1.5, 3.5, 7.5, 11.5, 15.5, 19.5, 23.5, 27.5 and 31.5 s.
const NON_INVITE_RESENDS: usize = 10;

/// The copies of a burst, past every ladder's re-sends.
const COPIES: usize = 15;

/// A strategy with no worker to take a new dialog: every new INVITE is
/// refused 503.
struct NoWorker;

#[async_trait]
impl RoutingStrategy for NoWorker {
    fn name(&self) -> &str {
        "NoWorker"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "no worker".into() })
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, _params: &'p RouteParams) -> Option<&'p str> {
        None
    }
}

/// Routes `msg` from `src` on `face` `copies` times, back to back, and counts
/// the answers whose status line starts `status`.
async fn burst(
    fx: &Fixture,
    msg: &SipMessage,
    src: SocketAddr,
    face: Face,
    copies: usize,
    status: &str,
) -> usize {
    for _ in 0..copies {
        fx.core.route_request(msg, src, face).await;
    }
    fx.take_sent().iter().filter(|(_, _, bytes)| bytes.starts_with(status.as_bytes())).count()
}

/// `msg`'s wire text with each `(from, to)` of `edits` replaced in turn, read
/// again.
fn rewritten(msg: &SipMessage, edits: &[(&str, &str)]) -> SipMessage {
    let text = String::from_utf8_lossy(msg.image()).to_string();
    parse_msg(&edits.iter().fold(text, |text, (from, to)| text.replacen(from, to, 1)))
}

/// A core with no worker, and the caller's INVITE it refused 503.
async fn refused() -> (Fixture, SipMessage) {
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let fx = dual_face_core_routing(reg, Arc::new(NoWorker), None);
    let invite = caller_invite("refused-copies@test");
    let refusal = burst(&fx, &invite, caller_src(), Face::External, 1, "SIP/2.0 503 ").await;
    assert_eq!(refusal, 1, "the INVITE is refused");
    (fx, invite)
}

/// A refused INVITE re-sent at wire speed: the copies Timer A re-sends draw
/// the 503, the rest nothing.
#[tokio::test(start_paused = true)]
async fn a_refused_invite_copied_at_wire_speed_draws_its_503_on_timer_as_schedule() {
    let (fx, invite) = refused().await;
    let repeated = burst(&fx, &invite, caller_src(), Face::External, COPIES, "SIP/2.0 503 ").await;
    assert_eq!(repeated, INVITE_RESENDS, "Timer A's re-sends");
}

/// The CANCEL of a refused INVITE re-sent at wire speed: it and the copies
/// Timer E re-sends draw a 200, the rest nothing.
#[tokio::test(start_paused = true)]
async fn the_cancel_of_a_refused_invite_copied_at_wire_speed_draws_200_on_timer_es_schedule() {
    let (fx, invite) = refused().await;
    let cancel =
        rewritten(&invite, &[("INVITE sip:", "CANCEL sip:"), ("CSeq: 1 INVITE", "CSeq: 1 CANCEL")]);
    let answered =
        burst(&fx, &cancel, caller_src(), Face::External, 1 + COPIES, "SIP/2.0 200 ").await;
    assert_eq!(answered, 1 + NON_INVITE_RESENDS, "the CANCEL and Timer E's re-sends");
}

/// `msg` re-sent at wire speed from `src` on `face`, each copy rejected at
/// preflight with `status`: the request and the copies its ladder re-sends
/// (`resends`) draw the reject, the rest nothing.
async fn rejected_on_the_ladder(
    msg: &SipMessage,
    src: SocketAddr,
    face: Face,
    status: &str,
    resends: usize,
) {
    let fx = dual_face_core();
    let answered = burst(&fx, msg, src, face, 1 + COPIES, status).await;
    assert_eq!(answered, 1 + resends, "{status}: the reject and the ladder's re-sends");
}

fn exhausted() -> SipMessage {
    rewritten(&caller_invite("preflight-copies@test"), &[("Max-Forwards: 70", "Max-Forwards: 0")])
}

#[tokio::test(start_paused = true)]
async fn an_invite_past_its_hops_copied_at_wire_speed_draws_483_on_timer_as_schedule() {
    rejected_on_the_ladder(
        &exhausted(),
        caller_src(),
        Face::External,
        "SIP/2.0 483 ",
        INVITE_RESENDS,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_bye_past_its_hops_copied_at_wire_speed_draws_483_on_timer_es_schedule() {
    let bye = rewritten(
        &exhausted(),
        &[
            ("INVITE sip:", "BYE sip:"),
            ("CSeq: 1 INVITE", "CSeq: 2 BYE"),
            ("To: <sip:bob@192.168.60.20>", "To: <sip:bob@192.168.60.20>;tag=callee-tag"),
        ],
    );
    rejected_on_the_ladder(&bye, caller_src(), Face::External, "SIP/2.0 483 ", NON_INVITE_RESENDS)
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_invite_looped_back_copied_at_wire_speed_draws_482_on_timer_as_schedule() {
    let own: SocketAddr = format!("{EXT_VIP}:{EXT_PORT}").parse().expect("an address");
    let invite = caller_invite("looped-copies@test");
    rejected_on_the_ladder(&invite, own, Face::Internal, "SIP/2.0 482 ", INVITE_RESENDS).await;
}

#[tokio::test(start_paused = true)]
async fn an_invite_requiring_an_extension_copied_at_wire_speed_draws_420_on_timer_as_schedule() {
    let invite = rewritten(
        &caller_invite("extension-copies@test"),
        &[("Max-Forwards: 70\r\n", "Max-Forwards: 70\r\nProxy-Require: x-unknown\r\n")],
    );
    rejected_on_the_ladder(&invite, caller_src(), Face::External, "SIP/2.0 420 ", INVITE_RESENDS)
        .await;
}
