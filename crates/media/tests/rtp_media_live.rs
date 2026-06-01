//! Slice 1 (live): play→record over real UDP + real wall-clock.
//!
//! The wire proof that the framing/codec/transport path works on real sockets,
//! not just the simulated fabric. Port of
//! `../sipjsserver/tests/media/rtp-media-live.test.ts`. Localhost UDP can drop a
//! packet, so the frame count is asserted with a small tolerance (unlike the
//! exact count in the simulated slice).

use std::sync::Arc;
use std::time::Duration;

use media::codec::{g711_round_trip, G711Codec};
use media::sdp::{MediaDirection, NegotiatedMedia};
use media::{MediaEndpoint, NetAddr, OpenOptions, PlayScript, StreamDirection, PCMA};
use sip_net::RealSignalingNetwork;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_play_record_over_real_udp() {
    let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(RealSignalingNetwork::new());
    // Real framing over real sockets; system clock for RTCP timestamps.
    let me = MediaEndpoint::new(net, Arc::new(media::rtp::HandRolled));

    // Port 0 → let the OS assign; read it back from local_addr.
    let alice = me.open("127.0.0.1", Some(0), OpenOptions::default()).await.unwrap();
    let bob = me.open("127.0.0.1", Some(0), OpenOptions::default()).await.unwrap();

    let a = alice.session("d1");
    a.configure(neg(bob.local_addr())).unwrap();
    a.commit(media::CommitReason::Confirmed);
    let b = bob.session("d1");
    b.configure(neg(alice.local_addr())).unwrap();
    b.commit(media::CommitReason::Confirmed);

    let clip = tone_clip(440.0, 8000); // 1 s = 50 frames
    a.play(PlayScript::Pcm(clip.clone()));

    // Real wall-clock: 50 frames * 20 ms = 1 s of pacing + delivery slack.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let rec = bob.session("d1").recorded();
    let expected = g711_round_trip(&clip, G711Codec::Pcma);
    let frames = clip.len().div_ceil(160) as u64;

    let inbound = bob
        .stats()
        .into_iter()
        .find(|s| s.direction == StreamDirection::Inbound)
        .expect("bob has an inbound stream");
    // Allow a little localhost loss.
    assert!(
        inbound.packets >= frames - 2,
        "got {} packets, expected ~{frames}",
        inbound.packets
    );
    // What did arrive must be a prefix of the expected round-trip stream.
    assert!(!rec.pcm.is_empty());
    assert_eq!(&rec.pcm[..], &expected[..rec.pcm.len().min(expected.len())]);
}
