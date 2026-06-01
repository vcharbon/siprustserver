//! Slice 3b: real RTP media through the real B2BUA.
//!
//! Port of `../sipjsserver/tests/media/basic-call-media.test.ts`. Alice and Bob
//! run real [`MediaTransport`]s; the SDP offer/answer is built by the production
//! offer/answer engine, carried over SIP through the **real** B2BUA, and applied
//! from whatever actually arrived. Because the Rust B2BUA is signalling-only (no
//! RTP relay), media flows peer-to-peer to the addresses the relayed SDP
//! advertises — so this exercises the same end-to-end path as the TS slice and
//! would catch a B2BUA that mangled the relayed c=/m=.
//!
//! The SIP harness runs on real wall-clock (not a paused runtime), so media uses
//! a separate simulated fabric and real sleeps for pacing.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::B2buaSut;
use media::sdp::{parse_sdp, OfferAnswerEngine};
use media::{CommitReason, NetAddr, OpenOptions, PlayScript, PCMA};
use media_harness::{classify, reference_clip, ClassifyOptions, ClipName};
use scenario_harness::Harness;
use sip_net::SimulatedSignalingNetwork;

const ALICE_RTP: u16 = 10000;
const BOB_RTP: u16 = 20000;

#[tokio::test]
async fn alice_and_bob_hear_each_other_through_b2bua() {
    let h = Harness::with_transit_delay("b2bua-basic-media", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5080", "127.0.0.1", 5070).await;

    // Media rides a separate simulated fabric (peer-to-peer; the B2BUA never
    // touches RTP). Both transports bind on the same media net so RTP routes.
    let media_net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(SimulatedSignalingNetwork::new(1));
    let me = media::ts_endpoint(media_net);
    let alice_rtp = me.open("127.0.0.1", Some(ALICE_RTP), OpenOptions::default()).await.unwrap();
    let bob_rtp = me.open("127.0.0.1", Some(BOB_RTP), OpenOptions::default()).await.unwrap();

    // Each side's offer/answer engine advertises its own RTP transport address.
    let mut alice_eng =
        OfferAnswerEngine::new(NetAddr::new("127.0.0.1", ALICE_RTP), vec![PCMA]);
    let mut bob_eng = OfferAnswerEngine::new(NetAddr::new("127.0.0.1", BOB_RTP), vec![PCMA]);

    // Alice offers (real SDP), sent through the B2BUA.
    let offer_wire = OfferAnswerEngine::to_wire(&alice_eng.local_offer());
    let mut call = alice.invite(&bob).with_sdp(&offer_wire).through(b2bua.addr).send().await;

    // Bob receives the relayed offer, answers it with his own SDP, and points
    // his media session at Alice.
    let mut uas = bob.receive("INVITE").await;
    let offer = parse_sdp(&String::from_utf8_lossy(&uas.request().body));
    let answer_wire = OfferAnswerEngine::to_wire(&bob_eng.answer_to(&offer).expect("bob answers"));
    let bob_session = bob_rtp.session("call");
    bob_session
        .configure(bob_eng.negotiated().expect("bob negotiated").clone())
        .unwrap();

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(&answer_wire).await;

    // Alice applies the relayed answer → points her media session at Bob.
    let ok = call.expect(200).await;
    let answer = parse_sdp(&String::from_utf8_lossy(&ok.body));
    let alice_negotiated = alice_eng.apply_remote(&answer, true).expect("alice applies answer");
    let alice_session = alice_rtp.session("call");
    alice_session.configure(alice_negotiated).unwrap();

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Both sides commit and emit RTP.
    alice_session.commit(CommitReason::Confirmed);
    bob_session.commit(CommitReason::Confirmed);
    alice_session.play(PlayScript::Pcm(reference_clip(ClipName::Alice)));
    bob_session.play(PlayScript::Pcm(reference_clip(ClipName::Bob)));

    // Real wall-clock: 2 s clip @ 20 ms = 100 frames ≈ 2 s of pacing.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Final-sweep verdicts: each peer hears the other's clip.
    let alice_hears = classify(&alice_session.recorded().pcm, &ClassifyOptions::default());
    let bob_hears = classify(&bob_session.recorded().pcm, &ClassifyOptions::default());
    assert_eq!(alice_hears.matched, Some(ClipName::Bob), "alice should hear bob");
    assert_eq!(bob_hears.matched, Some(ClipName::Alice), "bob should hear alice");

    // Teardown.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
