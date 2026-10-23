//! **A provisional on a leg the caller does not face is shown to no one.** A
//! service's media leg (unadopted, not the caller's peer) rings before it
//! answers. That 1xx creates an early dialog at this stack, the leg's UAC (RFC
//! 3261 §12.1.2), and a reliable one is this stack's to PRACK (RFC 3262 §4):
//! the caller is sent nothing, no caller-facing tag is mapped to it, and the
//! callee dialled after the media leg still rings the caller.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::decision::test_adapter::{route_to, route_to_with_18x};
use b2bua::decision::{NewCallResponse, RouteDecision, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, stated, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const DEST_SDP: &str = "v=0\r\no=dest 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const ORDINAL: &str = "w0";
const CALL_ID: &str = "media-leg-provisional@127.0.0.1";
const FROM_TAG: &str = "alice-tag";
/// The To-tag of the media server's early dialog, and of its answer.
const MRF_TAG: &str = "mrf-early";

/// How the media leg rings, and which relay the route arms.
#[derive(Clone, Copy)]
struct Shape {
    /// The `relayFirst18xTo180` strategy the route arms; `None` is the core relay.
    strategy: Option<RelayFirst18xStrategy>,
    /// The media server rings with this provisional.
    ring: Ring,
}

/// The provisional the media server rings with.
#[derive(Clone, Copy)]
enum Ring {
    /// A bare 180.
    Plain,
    /// A reliable 180 (RFC 3262) under this `RSeq`.
    Reliable(u32),
    /// A 183 carrying its SDP and an RFC 5009 early-media authorization.
    EarlyMedia,
}

/// A route that defers to the announcement service: a media leg to the media
/// server at `mrf` first, then the destination at `dest`.
fn decision(shape: Shape, mrf: SocketAddr, dest: SocketAddr) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r: RouteDecision = match shape.strategy {
                    Some(strategy) => {
                        route_to_with_18x(&dest.ip().to_string(), dest.port(), strategy)
                    }
                    None => route_to(&dest.ip().to_string(), dest.port()),
                };
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "clip-1",
                        "mrf_host": mrf.ip().to_string(),
                        "mrf_port": mrf.port(),
                        "dest_host": dest.ip().to_string(),
                        "dest_port": dest.port(),
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The media leg rings, then answers and plays; the destination rings and
/// answers; the caller hangs up. Checks the model the media leg's 1xx leaves.
async fn media_leg_rings_first(name: &str, shape: Shape) {
    let h = Harness::with_transit_delay(name, 1);
    if matches!(shape.ring, Ring::Reliable(_)) {
        h.waive(
            WaiverScope::rule(
                "reliable-needs-client-opt-in",
                "the media leg's INVITE withholds 100rel; the media server rings reliably all the \
                 same — the PRACK this stack owes such a provisional is the subject",
            )
            .on_party("mrf"),
        );
    }
    let alice = h.agent_auto("alice").await;
    let mrf = h.agent_auto("mrf").await;
    let dest = h.agent_auto("dest").await;
    let b2bua = B2buaSut::builder(decision(shape, mrf.addr(), dest.addr()))
        .services(vec![announcement::service()])
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&dest)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // ── the media leg rings: shown to no one ──
    let mut mrf_uas = mrf.receive("INVITE").await;
    let invite_cseq = mrf_uas.request().cseq().seq();
    match shape.ring {
        Ring::Reliable(rseq) => {
            mrf_uas.respond(180, "Ringing").with_to_tag(MRF_TAG).reliable(rseq).await;
            let mut prack = mrf.receive("PRACK").await;
            assert_eq!(
                (prack.request().to().tag(), stated(prack.request(), "RAck")),
                (Some(MRF_TAG), Some(format!("{rseq} {invite_cseq} INVITE"))),
                "this stack PRACKs the media leg's reliable 180 on its early dialog",
            );
            prack.respond(200, "OK").await;
        }
        Ring::Plain => mrf_uas.respond(180, "Ringing").with_to_tag(MRF_TAG).await,
        Ring::EarlyMedia => {
            mrf_uas
                .respond(183, "Session Progress")
                .with_to_tag(MRF_TAG)
                .with_header("P-Early-Media", "sendrecv")
                .with_sdp(MRF_SDP)
                .await
        }
    }
    settle_until(|| {
        b2bua
            .live_call(&call::derive_call_ref(ORDINAL, CALL_ID, FROM_TAG))
            .is_some_and(|c| c.cdr_events.len() >= 3)
    })
    .await;
    let live = b2bua
        .live_call(&call::derive_call_ref(ORDINAL, CALL_ID, FROM_TAG))
        .expect("the call is live");
    let media = live.b_legs.iter().find(|l| l.leg_id == "b-1").expect("the media leg");
    assert!(
        media.dialogs.iter().any(|d| d.peer_tag == MRF_TAG),
        "the media leg holds the early dialog its provisional created (RFC 3261 §12.1.2): {:?}",
        media.dialogs.iter().map(|d| &d.peer_tag).collect::<Vec<_>>(),
    );
    let fronts: Vec<_> = live.a_leg.dialogs.iter().flat_map(|d| d.fronts.iter()).collect();
    assert!(fronts.is_empty(), "no caller-facing dialog stands for it: {fronts:?}");

    // ── the media leg answers and plays; the caller gets its early media ──
    mrf_uas.respond(200, "OK").with_to_tag(MRF_TAG).with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    mrf_uas.adopt_to_tag(MRF_TAG);
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    done.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;

    // ── the destination rings the caller: the media leg took no ring of hers ──
    let mut dest_uas = dest.receive("INVITE").await;
    dest_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    dest_uas.respond(200, "OK").with_sdp(DEST_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Under the first-18x mask the media leg's 180 is not the caller's first
/// ring: it maps no tag, and the destination's 180 is the one relayed.
#[tokio::test(start_paused = true)]
async fn under_the_first_18x_mask_a_media_legs_180_maps_nothing() {
    let shape = Shape { strategy: Some(RelayFirst18xStrategy::DropSdp), ring: Ring::Plain };
    media_leg_rings_first("media-leg-180-first-18x-mask", shape).await;
}

/// On the core relay the media leg's 180 has no peer to reach: its early
/// dialog is still recorded.
#[tokio::test(start_paused = true)]
async fn on_the_core_relay_a_media_legs_180_keeps_its_early_dialog() {
    let shape = Shape { strategy: None, ring: Ring::Plain };
    media_leg_rings_first("media-leg-180-core-relay", shape).await;
}

/// A reliable 180 on the media leg reaches no one who would PRACK it: this
/// stack does (RFC 3262 §4).
#[tokio::test(start_paused = true)]
async fn a_media_legs_reliable_180_is_pracked_by_this_stack() {
    let shape = Shape { strategy: None, ring: Ring::Reliable(7) };
    media_leg_rings_first("media-leg-reliable-180", shape).await;
}

/// Under the early-media promotion a media leg's 183 answers no one: it is
/// absorbed like any provisional shown to no one, and the caller is answered
/// by the destination alone.
#[tokio::test(start_paused = true)]
async fn under_the_early_media_promotion_a_media_legs_183_answers_no_one() {
    let shape =
        Shape { strategy: Some(RelayFirst18xStrategy::PromotePemTo200), ring: Ring::EarlyMedia };
    media_leg_rings_first("media-leg-183-promotion", shape).await;
}
