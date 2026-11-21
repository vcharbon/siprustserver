//! A caller requiring `100rel` (RFC 3262 §3) on a call whose strategy keeps
//! the callee's provisionals from reaching her reliably: every strategy shows
//! her its own unreliable 180, and most withhold `100rel` toward the callee
//! besides. The stack cannot meet the requirement, so it answers her `420 Bad
//! Extension` naming `100rel` in `Unsupported` (RFC 3261 §8.2.2.3) before it
//! dials anyone. Each case ends with its CDR.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Alice requires `100rel` under `strategy`, offering where `offer`: she is
/// answered 420 naming it, and bob is sent nothing.
async fn refused_420(
    name: &str,
    ports: (u16, u16, u16),
    strategy: RelayFirst18xStrategy,
    offer: bool,
) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{}", ports.0)).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{}", ports.1)).await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", ports.1, strategy)
        .start(&h, "b2bua", &format!("127.0.0.1:{}", ports.2))
        .await;

    let invite = alice.invite(&bob).with_header("Require", "100rel");
    let invite = if offer { invite.with_sdp(OFFER) } else { invite };
    let mut call = invite.through(b2bua.addr).send().await;
    let refusal = call.expect(420).await;
    let unsupported: Vec<String> =
        refusal.raw(HeaderName::Unsupported).map(str::to_string).collect();
    h.advance(Duration::from_secs(1)).await;
    let dialled = bob.take_queued_inbound().await.is_some();
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(unsupported, ["100rel"], "the 420 names the extension");
    assert!(!dialled, "bob is sent nothing");
}

#[tokio::test(start_paused = true)]
async fn the_promotion_with_no_offer_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-promote",
        (5871, 5881, 5891),
        RelayFirst18xStrategy::PromotePemTo200,
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn fake_prack_with_no_offer_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-fake-prack",
        (5872, 5882, 5892),
        RelayFirst18xStrategy::FakePrack,
        false,
    )
    .await;
}

/// With an offer the promotion offers `100rel` toward the callee, yet shows
/// the caller an unreliable 180 of its own.
#[tokio::test(start_paused = true)]
async fn the_promotion_with_an_offer_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-promote-offer",
        (5875, 5885, 5895),
        RelayFirst18xStrategy::PromotePemTo200,
        true,
    )
    .await;
}

/// With an offer `fake-prack` acknowledges the callee's reliable provisionals
/// itself and shows the caller an unreliable 180.
#[tokio::test(start_paused = true)]
async fn fake_prack_with_an_offer_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-fake-prack-offer",
        (5876, 5886, 5896),
        RelayFirst18xStrategy::FakePrack,
        true,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn drop_sdp_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-drop-sdp",
        (5873, 5883, 5893),
        RelayFirst18xStrategy::DropSdp,
        true,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn keep_sdp_refuses_a_required_100rel() {
    refused_420(
        "required-100rel-keep-sdp",
        (5874, 5884, 5894),
        RelayFirst18xStrategy::KeepSdp,
        true,
    )
    .await;
}
