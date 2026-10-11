//! suppress-18x — what the strategy advertises toward the callee: the
//! reliability a rerouted leg is solicited, and a declared advertisement
//! narrowed by the strategy.

use super::*;

/// The leg a `/call/failure` reroute mints is solicited the same reliability
/// as the first: `drop-sdp` keeps alice unreliable and relays no PRACK, so
/// `100rel` she offered rides neither bob1's INVITE nor bob2's (RFC 3262 §3 —
/// an offer this stack cannot honour is not made), whichever mint assembled
/// the message.
#[tokio::test]
async fn the_rerouted_leg_is_solicited_the_same_reliability_as_the_first() {
    let h = Harness::with_transit_delay("suppress-18x-reroute-same-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:5643").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5644").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5645").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        "127.0.0.1",
        5644,
        5645,
        "sip:+1234@127.0.0.1:5645",
        RelayFirst18xStrategy::DropSdp,
    )
    .start(&h, "b2bua", "127.0.0.1:5646")
    .await;

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .through(b2bua.addr)
        .send()
        .await;

    let mut uas1 = bob1.receive("INVITE").await;
    assert!(
        !has_token(uas1.request().header::<Supported>(), "100rel"),
        "100rel withheld from bob1's Supported",
    );
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;

    // The rerouted leg: the same withhold, the rest of alice's set relayed.
    let mut uas2 = bob2.receive("INVITE").await;
    let supported = uas2.request().header::<Supported>().expect("a Supported line").unwrap();
    assert!(!supported.contains("100rel"), "100rel withheld from bob2's Supported");
    assert!(supported.contains("timer"), "the rest of alice's set rides bob2's INVITE");

    uas2.respond(180, "Ringing").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A declared advertisement toward the callee stands under `drop-sdp`, narrowed
/// by the strategy's withhold rather than replaced by alice's relayed set: bob
/// is told `timer` — the declaration — and not `100rel`, whichever of the two
/// stated it.
#[tokio::test]
async fn a_declared_advertisement_is_narrowed_by_the_strategy_not_discarded() {
    let h = Harness::with_transit_delay("suppress-18x-declared-advert", 0);
    let alice = h.agent("alice", "127.0.0.1:5647").await;
    let bob = h.agent("bob", "127.0.0.1:5648").await;
    let b2bua = B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to_with_18x("127.0.0.1", 5648, RelayFirst18xStrategy::DropSdp);
                r.features.advertise_capabilities =
                    Some(call::features::AdvertiseCapabilitiesFeature {
                        toward_originator: None,
                        toward_originated: Some(call::features::AdvertisedCapabilities {
                            allow: None,
                            supported: Some(vec!["timer".to_string(), "100rel".to_string()]),
                        }),
                    });
                NewCallResponse::Route(r)
            })
            .build(),
    ))
    .start(&h, "b2bua", "127.0.0.1:5649")
    .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, replaces")
        .through(b2bua.addr)
        .send()
        .await;

    let mut uas = bob.receive("INVITE").await;
    let supported = uas.request().header::<Supported>().expect("a Supported line").unwrap();
    assert_eq!(
        supported.iter().collect::<Vec<_>>(),
        ["timer"],
        "the declaration, minus the withheld tag"
    );

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
