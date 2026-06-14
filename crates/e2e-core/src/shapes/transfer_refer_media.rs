//! The `transfer-refer-media` Callflow shape: a **blind call transfer via SIP
//! REFER** with RTP media re-exchange (Phase L). alice↔bob1 is established; bob1
//! then REFERs the call to bob2; the SUT authorizes the transfer (the scripted
//! `/call/refer` backend keyed on `X-Api-Call.refer_key`), drives the C-leg
//! INVITE to bob2 + the implicit-subscription NOTIFYs (`Event: refer`), and
//! realigns both legs — a re-INVITE toward bob2 carrying alice's offer, then a
//! re-INVITE toward alice carrying bob2's answer. Once A↔bob2 is bridged the two
//! exchange real audio and each records what it heard: `alice hears bob2` /
//! `bob2 hears alice`, proving media survived the transfer.
//!
//! The signaling SDP is cosmetic (it satisfies the RFC body checks); the actual
//! RTP rides [`negotiate_call`] on the raw fabric, **re-negotiated** for the
//! post-transfer A↔bob2 dialog (each `negotiate_call` is independent — a fresh
//! offer/answer engine per `dialog_id`). The driving sequence mirrors
//! `b2bua-harness/tests/refer_full_transfer.rs` (`refer_allow_full_happy`).

use std::time::Duration;

use async_trait::async_trait;
use media::{OpenOptions, PlayScript};
use media_harness::{ClipName, NegotiateOptions, negotiate_call, reference_clip};
use sip_message::generators::InDialogMethod;

use crate::egress::ApiCall;
use crate::infra::InfraRuntime;
use crate::media::MediaCapture;
use crate::model::Input;
use crate::shape::{Anchor, CallflowShape, MediaMode};

const ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Refer,
    Anchor::ReInvite,
    Anchor::Bye,
];

/// How long A and bob2 talk after the transfer completes, before the BYE. The
/// reference clips are 2 s; the classifier needs ≥100 ms of voiced audio, so
/// 1.5 s is a wide margin that stays fast on the real clock.
const TALK_MS: u64 = 1_500;

pub struct TransferReferMedia;

/// A sendrecv PCMA/PCMU offer/answer describing `port` on `ip`. `sendrecv` keeps
/// the b2bua's one-way-audio guard happy on the realign re-INVITEs.
fn sdp(name: &str, ip: &str, port: u16) -> String {
    format!(
        "v=0\r\no={name} 1 1 IN IP4 {ip}\r\ns=-\r\nc=IN IP4 {ip}\r\nt=0 0\r\n\
         m=audio {port} RTP/AVP 8 0\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n"
    )
}

#[async_trait(?Send)]
impl CallflowShape for TransferReferMedia {
    fn id(&self) -> &str {
        "transfer-refer-media"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }
    fn agents(&self) -> &[&str] {
        &["alice", "bob1", "bob2"]
    }
    fn media(&self) -> MediaMode {
        MediaMode::Exchange
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        // RTP transports on the raw (un-recorded) fabric; the SDP on the SIP
        // wire mirrors the ports cosmetically. Only alice + bob2 talk (the call
        // is media-checked AFTER the transfer), so bob1 needs no transport.
        let me = media::ts_endpoint(rt.raw_network());
        let a_rtp = rt.cfg.media_addr("alice", 40600);
        let b2_rtp = rt.cfg.media_addr("bob2", 40604);
        let alice_t = me
            .open(&a_rtp.ip().to_string(), Some(a_rtp.port()), OpenOptions::default())
            .await
            .expect("open alice RTP");
        let bob2_t = me
            .open(&b2_rtp.ip().to_string(), Some(b2_rtp.port()), OpenOptions::default())
            .await
            .expect("open bob2 RTP");
        let a_sdp = sdp("alice", &a_rtp.ip().to_string(), a_rtp.port());
        let b1_sdp = sdp("bob1", &a_rtp.ip().to_string(), 40602);
        let b2_sdp = sdp("bob2", &b2_rtp.ip().to_string(), b2_rtp.port());

        // The REFER target, resolved generically by the layout (same primitive as
        // any callee): its URI is the Refer-To, its address the `X-Api-Call`
        // transfer destination. The C-leg egresses through the LB by R-URI exactly
        // like the rerouting shape's bob2 leg.
        let bob2_target = rt.callee("bob2");

        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");
        let bob2 = rt.agent("bob2");

        // ── A↔bob1 established ────────────────────────────────────────────
        // The layout realizes alice's INVITE on its wire + applies Test case
        // overrides (see `basic_call.rs`).
        let invite = rt.outgoing_invite(&["bob1"], input, alice.invite(bob1).with_sdp(&a_sdp));
        let mut call = invite.send().await;

        let mut uas = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas.request());
        uas.respond(180, "Ringing").await;
        call.expect(180).await;
        uas.respond(200, "OK").with_sdp(&b1_sdp).await;
        let answer = call.expect(200).await;
        rt.anchor("alice", Anchor::Answer, &answer);
        let mut alice_dialog = call.ack().await;
        let ack = bob1.receive("ACK").await;
        rt.anchor("bob1", Anchor::Ack, ack.request());

        // ── bob1 REFERs the call to bob2 → 202 Accepted ───────────────────
        let mut bob1_dialog = uas.dialog();
        let refer_to = format!("<{}>", bob2_target.uri);
        let x_api_refer = ApiCall::refer(
            "refer-allow-c",
            bob2_target.addr.ip().to_string(),
            bob2_target.addr.port(),
        )
        .to_header();
        let mut refer = bob1_dialog
            .send_request(InDialogMethod::Refer)
            .with_header("Refer-To", &refer_to)
            .with_header("X-Api-Call", &x_api_refer)
            .send()
            .await;
        refer.expect(202).await;

        // Implicit subscription: NOTIFY 100 active (C being dialed).
        let mut n100 = bob1.receive("NOTIFY").await;
        n100.respond(200, "OK").await;

        // ── C-leg: bob2 answers the initial transfer INVITE ───────────────
        let mut bob2_uas = bob2.receive("INVITE").await;
        rt.anchor("bob2", Anchor::InitialInvite, bob2_uas.request());
        bob2_uas.respond(200, "OK").with_sdp(&b2_sdp).await;
        bob2.receive("ACK").await;

        // NOTIFY terminated (C answered) → transfer succeeded.
        let mut nterm = bob1.receive("NOTIFY").await;
        nterm.respond(200, "OK").await;

        // ── c-realign: re-INVITE bob2 with alice's offer ──────────────────
        let mut c_realign = bob2.receive("INVITE").await;
        rt.anchor("bob2", Anchor::ReInvite, c_realign.request());
        c_realign.respond(200, "OK").with_sdp(&b2_sdp).await;
        bob2.receive("ACK").await;

        // ── a-realign: re-INVITE alice with bob2's answer → merge(a,c) ─────
        let mut a_realign = alice.receive("INVITE").await;
        rt.anchor("alice", Anchor::ReInvite, a_realign.request());
        a_realign.respond(200, "OK").with_sdp(&a_sdp).await;
        alice.receive("ACK").await;

        // ── A↔bob2 bridged: re-negotiate media and talk ───────────────────
        let session = negotiate_call(
            &alice_t,
            &bob2_t,
            NegotiateOptions { dialog_id: Some("a-c".into()), ..Default::default() },
        )
        .expect("media offer/answer A↔bob2");
        session.alice_session.play(PlayScript::Pcm(reference_clip(ClipName::Alice)));
        session.bob_session.play(PlayScript::Pcm(reference_clip(ClipName::Bob)));
        tokio::time::sleep(Duration::from_millis(TALK_MS)).await;
        rt.push_media(MediaCapture {
            agent: "alice".into(),
            expected: ClipName::Bob,
            pcm: session.alice_session.recorded().pcm,
        });
        rt.push_media(MediaCapture {
            agent: "bob2".into(),
            expected: ClipName::Alice,
            pcm: session.bob_session.recorded().pcm,
        });

        // ── Teardown: A BYE reaches bob2 (peered); the orphaned bob1 is torn
        // down by begin-termination ──────────────────────────────────────
        let mut alice_bye = alice_dialog.bye().await;
        let mut bye_c = bob2.receive("BYE").await;
        rt.anchor("bob2", Anchor::Bye, bye_c.request());
        bye_c.respond(200, "OK").await;
        bob1.receive("BYE").await.respond(200, "OK").await;
        alice_bye.expect(200).await;
    }
}
