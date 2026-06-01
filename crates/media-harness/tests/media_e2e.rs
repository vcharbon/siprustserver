//! Slice 3a: end-to-end media over the shared simulated fabric.
//!
//! Port of `../sipjsserver/tests/media/media-e2e.test.ts`. A transparent relay →
//! both peers hear each other; an SDP-rewrite bug that corrupts the connection
//! address → the victim hears silence/no-audio and has zero inbound sources,
//! while the misdirected media physically lands at the wrong port (proving the
//! corruption is targeted, not a total drop).

use std::sync::Arc;
use std::time::Duration;

use media::{OpenOptions, PlayScript};
use media_harness::testkit::advance_media;
use media_harness::{
    classify, corrupt_connection_addr, negotiate_call, reference_clip, Classification, ClipName,
    ClassifyOptions, NegotiateOptions,
};
use sip_net::SimulatedSignalingNetwork;

fn pcm(name: ClipName) -> PlayScript {
    PlayScript::Pcm(reference_clip(name))
}

#[tokio::test(start_paused = true)]
async fn transparent_relay_both_hear_each_other() {
    let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(SimulatedSignalingNetwork::new(1));
    let me = media::ts_endpoint(net);
    let alice = me.open("10.10.0.1", Some(40000), OpenOptions::default()).await.unwrap();
    let bob = me.open("10.20.0.1", Some(40002), OpenOptions::default()).await.unwrap();

    let call = negotiate_call(&alice, &bob, NegotiateOptions::default()).expect("negotiate");

    call.alice_session.play(pcm(ClipName::Alice));
    call.bob_session.play(pcm(ClipName::Bob));
    advance_media(Duration::from_millis(3000)).await;

    let alice_hears = classify(&call.alice_session.recorded().pcm, &ClassifyOptions::default());
    let bob_hears = classify(&call.bob_session.recorded().pcm, &ClassifyOptions::default());
    assert_eq!(alice_hears.matched, Some(ClipName::Bob), "alice should hear bob");
    assert_eq!(bob_hears.matched, Some(ClipName::Alice), "bob should hear alice");
}

#[tokio::test(start_paused = true)]
async fn sdp_rewrite_bug_misdirects_media() {
    let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(SimulatedSignalingNetwork::new(1));
    let me = media::ts_endpoint(net);
    let alice = me.open("10.10.0.1", Some(41000), OpenOptions::default()).await.unwrap();
    let bob = me.open("10.20.0.1", Some(41002), OpenOptions::default()).await.unwrap();

    // Corrupt the answer Alice observes → Alice sends her media to a bogus IP.
    let call = negotiate_call(
        &alice,
        &bob,
        NegotiateOptions {
            rewrite_answer: Some(corrupt_connection_addr("10.99.99.99")),
            ..Default::default()
        },
    )
    .expect("negotiate");

    call.alice_session.play(pcm(ClipName::Alice));
    call.bob_session.play(pcm(ClipName::Bob));
    advance_media(Duration::from_millis(3000)).await;

    // Bob never hears Alice: her RTP went to 10.99.99.99, not to Bob.
    let bob_hears = classify(&call.bob_session.recorded().pcm, &ClassifyOptions::default());
    assert_ne!(bob_hears.matched, Some(ClipName::Alice));
    assert!(matches!(
        bob_hears.classification,
        Classification::Silence | Classification::NoAudio
    ));
    assert_eq!(bob.sources().len(), 0, "bob should have no inbound sources");

    // The misdirection is targeted: Bob's own media still reached Alice's port
    // (Bob answered with his real address), so Alice has a live inbound source.
    assert!(
        !alice.sources().is_empty(),
        "alice should still receive bob's media"
    );
}
