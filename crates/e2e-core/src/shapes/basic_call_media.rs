//! The `basic-call-media` Callflow shape: the `basic-call` signaling flow with
//! REAL RTP audio (Phase J). Each side opens a `MediaEndpoint` on the same
//! fabric as the signaling (below the SIP recording decorators), the
//! INVITE/200 carry SDP describing the actual RTP ports, and once the call is
//! up each side streams its deterministic reference clip (Alice 200 Hz /
//! Bob 110 Hz) while recording what it hears. The captures land on the
//! [`InfraRuntime`] for the executor to fold into `.wav` artifacts + "hears"
//! check verdicts. Advance-free: the talk window is a plain `sleep`, which
//! auto-advances under a paused clock and is a real 1.5 s under a wall clock.

use std::time::Duration;

use async_trait::async_trait;
use media::{OpenOptions, PlayScript};
use media_harness::{reference_clip, ClipName, NegotiateOptions, negotiate_call};

use crate::infra::InfraRuntime;
use crate::media::MediaCapture;
use crate::model::Input;
use crate::shape::{Anchor, CallflowShape, MediaMode};

const ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];

/// How long both sides talk before the BYE. The reference clips are 2 s;
/// classification needs ≥100 ms of voiced audio, so 1.5 s gives a wide margin
/// while keeping the real-clock variant fast.
const TALK_MS: u64 = 1_500;

pub struct BasicCallMedia;

fn sdp(name: &str, ip: &str, port: u16) -> String {
    format!(
        "v=0\r\no={name} 1 1 IN IP4 {ip}\r\ns=-\r\nc=IN IP4 {ip}\r\nt=0 0\r\nm=audio {port} RTP/AVP 8 0\r\n"
    )
}

#[async_trait(?Send)]
impl CallflowShape for BasicCallMedia {
    fn id(&self) -> &str {
        "basic-call-media"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }
    fn agents(&self) -> &[&str] {
        &["alice", "bob1"]
    }
    fn media(&self) -> MediaMode {
        MediaMode::Exchange
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        // RTP transports on the raw fabric; SDP in the signaling mirrors them.
        let me = media::ts_endpoint(rt.raw_network());
        let a_rtp = rt.cfg.media_addr("alice", 40600);
        let b_rtp = rt.cfg.media_addr("bob1", 40602);
        let alice_t = me
            .open(&a_rtp.ip().to_string(), Some(a_rtp.port()), OpenOptions::default())
            .await
            .expect("open alice RTP");
        let bob_t = me
            .open(&b_rtp.ip().to_string(), Some(b_rtp.port()), OpenOptions::default())
            .await
            .expect("open bob1 RTP");
        let session = negotiate_call(&alice_t, &bob_t, NegotiateOptions::default())
            .expect("media offer/answer");

        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");

        // The layout realizes the logical INVITE on its wire (X-Api-Call pin /
        // AOR R-URI / nothing) and applies the Test case overrides — see
        // `basic_call.rs`. The SDP describes alice's real RTP port either way.
        let invite = rt.outgoing_invite(
            &["bob1"],
            input,
            alice.invite(bob1).with_sdp(&sdp("alice", &a_rtp.ip().to_string(), a_rtp.port())),
        );
        let mut call = invite.send().await;

        let mut uas = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas.request());
        uas.respond(180, "Ringing").await;
        call.expect(180).await;

        uas.respond(200, "OK")
            .with_sdp(&sdp("bob", &b_rtp.ip().to_string(), b_rtp.port()))
            .await;
        let answer = call.expect(200).await;
        rt.anchor("alice", Anchor::Answer, &answer);

        let mut dialog = call.ack().await;
        let ack = bob1.receive("ACK").await;
        rt.anchor("bob1", Anchor::Ack, ack.request());

        // Call is up: both sides talk, then we capture what each one heard.
        session.alice_session.play(PlayScript::Pcm(reference_clip(ClipName::Alice)));
        session.bob_session.play(PlayScript::Pcm(reference_clip(ClipName::Bob)));
        tokio::time::sleep(Duration::from_millis(TALK_MS)).await;
        rt.push_media(MediaCapture {
            agent: "alice".into(),
            expected: ClipName::Bob,
            pcm: session.alice_session.recorded().pcm,
        });
        rt.push_media(MediaCapture {
            agent: "bob1".into(),
            expected: ClipName::Alice,
            pcm: session.bob_session.recorded().pcm,
        });

        let mut bye = dialog.bye().await;
        let mut bye_uas = bob1.receive("BYE").await;
        rt.anchor("bob1", Anchor::Bye, bye_uas.request());
        bye_uas.respond(200, "OK").await;
        bye.expect(200).await;
    }
}
