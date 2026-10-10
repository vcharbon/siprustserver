use super::*;
use crate::rules::Obligation;
use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

mod answer;
mod description;
mod revision;
mod round;

const ALICE: &str = "10.0.0.1:5060";
const BOB: &str = "10.0.0.2:5070";

const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

/// The harness default body that must never ride an ACK: a fresh session
/// (`o=bob 1 1`) offering audio over RTP/AVP.
const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

/// The head a synthetic message carries: `extra` lines, and the
/// `Content-Type` a body demands (RFC 3261 §20.15) — `application/sdp`,
/// which is what every description these tests carry declares.
fn head(extra: &str, body: Option<&str>) -> Vec<u8> {
    let declared = match body {
        Some(b) if !b.is_empty() => "Content-Type: application/sdp\r\n",
        _ => "",
    };
    format!("X: y\r\n{extra}{declared}\r\n").into_bytes()
}

fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    cseq: u32,
    to_tag: Option<&str>,
    body: Option<&str>,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "cid-1@host".to_string(),
        cseq,
        cseq_method: method.to_string(),
        via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
        from_tag: Some("at".to_string()),
        to_tag: to_tag.map(str::to_string),
        head: Some(head("", body)),
        body: Some(body.unwrap_or("").as_bytes().to_vec()),
    }
}

fn resp(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    method: &str,
    body: Option<&str>,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "cid-1@host".to_string(),
        cseq,
        cseq_method: method.to_string(),
        via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        head: Some(head("", body)),
        body: Some(body.unwrap_or("").as_bytes().to_vec()),
    }
}

fn view(msgs: &[Msg]) -> (Observation, &[Msg]) {
    (Observation { closed: true, ..Observation::default() }, msgs)
}

fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    let (obs, msgs) = view(msgs);
    rule.eval(&WireView { msgs, obs: &obs })
}

fn violations(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    run(rule, msgs).into_iter().filter(Finding::violated).collect()
}

/// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
/// 200(answer) → ACK. The ACK bodies are the caller's choice.
fn t38_call(first_ack: Option<&str>, second_ack: Option<&str>) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 180, 1, "INVITE", Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", None),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), first_ack),
        req(5, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
        resp(6, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)),
        req(7, ALICE, BOB, "ACK", 3, Some("bt"), second_ack),
    ]
}

// ---- the RFC 3264 offer/answer-model family -------------------------

/// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

/// Its answer: same stream table, same `t=`, same payload-type bindings.
const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

const OFFER_2MEDIA: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

/// A response presenting a chosen early dialog — the To tag that tells two
/// forks of one INVITE apart.
fn resp_fork(at_us: u64, status: u16, cseq: u32, to_tag: &str, body: Option<&str>) -> Msg {
    Msg {
        to_tag: Some(to_tag.to_string()),
        ..resp(at_us, BOB, ALICE, status, cseq, "INVITE", body)
    }
}

/// INVITE(offer) → 200(answer) → ACK: one round, alice offering.
fn one_round(offer: &str, answer: &str) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(offer)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(answer)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ]
}
