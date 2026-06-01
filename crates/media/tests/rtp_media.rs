//! Slice 1 (transport half): play→record over the simulated fabric.
//!
//! Port of the play→record suite in
//! `../sipjsserver/tests/media/rtp-media.test.ts`, for both framing flavors.
//! Rather than depend on the spectral classifier (slice 0, in `media-harness`),
//! this asserts the transport/codec path directly: the PCM Bob records must
//! equal the G.711 round-trip of the clip Alice played, and the packet/byte
//! stats must be exact. RTCP SR/RR counts must flow on the configured interval.

use std::sync::Arc;
use std::time::Duration;

use media::codec::{g711_round_trip, G711Codec};
use media::sdp::{MediaDirection, NegotiatedMedia};
use media::{MediaEndpoint, NetAddr, OpenOptions, PlayScript, StreamDirection, PCMA};
use sip_net::SimulatedSignalingNetwork;

mod common;
use common::advance_media;

const PTIME_SAMPLES: usize = 160; // 20 ms @ 8 kHz

/// A deterministic 1 s tone clip (no RNG), distinct enough to compare bit-for-bit
/// after G.711 transcoding.
fn tone_clip(freq_hz: f64, samples: usize) -> Vec<i16> {
    (0..samples)
        .map(|n| {
            let t = n as f64 / 8000.0;
            (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16
        })
        .collect()
}

fn neg(remote: &NetAddr) -> NegotiatedMedia {
    NegotiatedMedia {
        remote: remote.clone(),
        codec: PCMA,
        direction: MediaDirection::SendRecv,
        send: true,
        receive: true,
    }
}

async fn play_record(endpoint_of: fn(Arc<dyn sip_net::SignalingNetwork>) -> MediaEndpoint) {
    let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(SimulatedSignalingNetwork::new(1));
    let me = endpoint_of(net);

    let alice = me.open("10.10.0.1", Some(40000), OpenOptions::default()).await.unwrap();
    let bob = me.open("10.20.0.1", Some(40002), OpenOptions::default()).await.unwrap();

    let a = alice.session("d1");
    a.configure(neg(bob.local_addr())).unwrap();
    a.commit(media::CommitReason::Confirmed);
    let b = bob.session("d1");
    b.configure(neg(alice.local_addr())).unwrap();
    b.commit(media::CommitReason::Confirmed);

    let clip = tone_clip(440.0, 8000); // 1 s
    a.play(PlayScript::Pcm(clip.clone()));

    // 1 s clip @ 20 ms = 50 frames; advance well past send + transit.
    advance_media(Duration::from_millis(2000)).await;

    let rec = bob.session("d1").recorded();
    assert_eq!(rec.sample_rate, 8000);
    // Bob hears exactly the A-law round-trip of Alice's clip.
    let expected = g711_round_trip(&clip, G711Codec::Pcma);
    assert_eq!(rec.pcm, expected, "recorded PCM != played clip (A-law round-trip)");

    let expected_frames = clip.len().div_ceil(PTIME_SAMPLES) as u64;
    let inbound = bob
        .stats()
        .into_iter()
        .find(|s| s.direction == StreamDirection::Inbound)
        .expect("bob has an inbound stream");
    assert_eq!(inbound.packets, expected_frames);
    assert_eq!(inbound.payload_type, PCMA.payload_type);
    assert_eq!(inbound.bytes, expected_frames * (12 + PTIME_SAMPLES as u64));
}

#[tokio::test(start_paused = true)]
async fn play_record_handrolled() {
    play_record(media::ts_endpoint).await;
}

#[tokio::test(start_paused = true)]
async fn play_record_webrtc_witness() {
    play_record(media::webrtc_endpoint).await;
}

#[tokio::test(start_paused = true)]
async fn rtcp_reports_flow_on_the_interval() {
    let net = Arc::new(SimulatedSignalingNetwork::new(1));
    let me = media::ts_endpoint(net);
    let opts = OpenOptions {
        rtcp_interval_ms: Some(1000),
        ..Default::default()
    };
    let alice = me.open("10.10.0.1", Some(41000), opts.clone()).await.unwrap();
    let bob = me.open("10.20.0.1", Some(41002), opts).await.unwrap();

    let a = alice.session("d1");
    a.configure(neg(bob.local_addr())).unwrap();
    a.commit(media::CommitReason::Confirmed);
    let b = bob.session("d1");
    b.configure(neg(alice.local_addr())).unwrap();
    b.commit(media::CommitReason::Confirmed);

    a.play(PlayScript::Pcm(tone_clip(440.0, 8000)));
    advance_media(Duration::from_millis(4000)).await;

    let alice_out = alice
        .stats()
        .into_iter()
        .find(|s| s.direction == StreamDirection::Outbound)
        .expect("alice outbound");
    assert!(alice_out.rtcp_packets_sent > 0, "alice should have sent SRs");
    let bob_in = bob
        .stats()
        .into_iter()
        .find(|s| s.direction == StreamDirection::Inbound)
        .expect("bob inbound");
    assert!(
        bob_in.rtcp_packets_received > 0,
        "bob should have received RTCP"
    );
}
