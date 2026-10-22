//! The LIVE policy this adapter adds to the merged bodies: subject
//! dispatch, the relay-lane skip, and the wire position a finding anchors
//! on. The rule semantics themselves are pinned in `rfc_rules::rules`.

use std::collections::HashSet;

use super::*;
use crate::rfc_audit::{evaluate_rfc_findings, RfcFinding};
use crate::types::{BindSummary, UdpPacket};

mod ack;
mod cancel;
mod capability;
mod correlation;
mod cseq;
mod dialog;
mod final_response;
mod offer_answer;
mod prack;
mod proxy;
mod register;
mod reinvite;
mod retransmit;
mod via;
mod wellformed;

// `to_sip_entries` resolves lanes by ADDRESS, so the traces use addr bind
// keys: the SUT answering at :5080, the caller at :5060, the callee at :5070.
const SUT: &str = "127.0.0.1:5080";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const A: &str = "sip:alice@127.0.0.1";
const B: &str = "sip:bob@127.0.0.1";

/// The one live row of `id`.
fn live(id: RuleId) -> Live {
    let [only] = rows_of(id)[..] else { panic!("{id} has more than one live row") };
    only
}

/// The live row of `id` at severity `advisory`, for a rule surfaced twice.
fn live_arm(id: RuleId, advisory: bool) -> Live {
    rows_of(id).into_iter().find(|l| l.advisory == advisory).expect("a row at that severity")
}

fn sent(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
    Stamped {
        event: SignalingNetworkEvent::SendCalled {
            bind_key: bind.to_string(),
            to: to.parse().unwrap(),
            msg: raw,
        },
        seq,
        at_ms: seq,
    }
}

fn recv(bind: &str, raw: Vec<u8>, src: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
    Stamped {
        event: SignalingNetworkEvent::RecvItem {
            bind_key: bind.to_string(),
            disposition: crate::types::RecvDisposition::Delivered,
            wire: crate::contracts::WireStamp::of_bytes(&raw),
            packet: UdpPacket::unparsed(raw, src.parse().unwrap(), seq),
        },
        seq,
        at_ms: seq,
    }
}

/// A `BindAcquire` declaring `roles` for `bind` — what subject dispatch in
/// [`evaluate_rfc_findings`] reads.
fn bind_roles(bind: &str, roles: HashSet<UaRole>, seq: u64) -> Stamped<SignalingNetworkEvent> {
    Stamped {
        event: SignalingNetworkEvent::BindAcquire {
            bind_key: bind.to_string(),
            summary: BindSummary {
                addr: bind.parse().unwrap(),
                queue_max: 256,
                reuse_port: false,
                roles,
                has_pre_ingress: false,
            },
        },
        seq,
        at_ms: seq,
    }
}

fn req(method: &str, branch: &str, cseq: u32, ttag: Option<&str>) -> Vec<u8> {
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, cseq: u32, method: &str, ttag: &str, branch: &str) -> Vec<u8> {
    // Empty `ttag` ⇒ a tagless To (a 100 Trying); `;tag=` with an empty
    // value is not a wire token and even the lenient parser rejects it.
    let to = if ttag.is_empty() { format!("<{B}>") } else { format!("<{B}>;tag={ttag}") };
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The b-leg INVITE a B2BUA originates, carrying its OWN Call-ID — each leg
/// is a distinct dialog, which is what keeps a B2BUA lane out of the relay
/// classification that exempts a forwarding proxy.
fn b_leg_invite(branch: &str) -> Vec<u8> {
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A request carrying an explicit header block — the §9.1 Route rules read
/// the bytes, so a builder that can vary them is what those tests need.
fn req_with(method: &str, branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An in-dialog request the SUT sent, with caller-chosen Request-URI,
/// From/To URIs and extra header rows — the §12 rules read those off the
/// bytes, so a builder that can vary them is what those tests need.
#[allow(clippy::too_many_arguments)]
fn in_dialog(
    method: &str,
    r_uri: &str,
    branch: &str,
    cseq: u32,
    from_uri: &str,
    to_uri: &str,
    extra: &str,
) -> Vec<u8> {
    format!(
        "{method} {r_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{from_uri}>;tag=at\r\n\
             To: <{to_uri}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A response carrying extra header rows (Record-Route, Allow, …).
fn resp_with(status: u16, cseq: u32, method: &str, branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The dialog-creating INVITE (no To tag) the SUT sends.
fn in_dialog_less_to_tag(branch: &str) -> Vec<u8> {
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[test]
fn proxy_declared_lane_is_out_of_subject() {
    // Subject dispatch: the same trace fires for a `{Uac, Uas}` bind and is
    // silent for a `{Proxy}`-declared one.
    let wire = |bind: &str| {
        vec![
            recv(bind, req("INVITE", "z9hG4bK-s", 1, None), ALICE, 1),
            sent(bind, resp(487, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 2),
            recv(bind, req("ACK", "z9hG4bK-s", 1, Some("bt")), ALICE, 3),
            sent(bind, resp(603, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 4),
        ]
    };
    let fired = |evs: &[Stamped<SignalingNetworkEvent>]| -> Vec<RfcFinding> {
        evaluate_rfc_findings(evs)
            .into_iter()
            .filter(|f| f.rule == rfc_rules::RuleId::SingleFinalPerServerTxn.token())
            .collect()
    };

    let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
    ua.extend(wire(SUT));
    let found = fired(&ua);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(!found[0].advisory, "the rule gates");

    let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
    proxy.extend(wire(SUT));
    assert!(fired(&proxy).is_empty(), "a {{Proxy}} bind is not the subject");
}

/// Every merged rule reaches the live suite. A rung that gives a rule a
/// body and forgets to surface it here would otherwise run nowhere and say
/// nothing — the adapter is the ONLY live consumer, so the two registries
/// are one fact stated twice.
///
/// One token may carry SEVERAL adapters: a live policy that waives part of
/// an obligation states the waived arm and the gating arm separately, and
/// they answer to the same rule. Two adapters agreeing on severity are a
/// duplicate, not a split, and the rung says so.
#[test]
fn every_merged_rule_has_a_live_adapter() {
    let mut by_token: std::collections::BTreeMap<&str, Vec<bool>> =
        std::collections::BTreeMap::new();
    for rule in cross_rules() {
        by_token.entry(rule.name()).or_default().push(rule.force_advisory());
    }
    for (token, mut severities) in by_token.clone() {
        let arms = severities.len();
        severities.sort_unstable();
        severities.dedup();
        assert_eq!(arms, severities.len(), "{token} is surfaced twice at one severity");
    }
    let missing: Vec<&str> = rfc_rules::RuleId::ALL
        .iter()
        .map(|r| r.token())
        .filter(|t| !by_token.contains_key(t))
        .collect();
    assert!(
        missing.is_empty(),
        "rules with no live row (add one to the LIVE table of wire_adapter/<family>.rs): {}",
        missing.join(", ")
    );
}

/// A request the CALLEE originates: From/To reversed relative to the
/// establishing INVITE, which is the orientation a callee-initiated BYE or
/// re-INVITE rides.
fn from_callee(method: &str, branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "{method} {A} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <{B}>;tag=bt\r\n\
             To: <{A}>;tag=at\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

//
// Fixtures that can carry the reliable-provisional headers, an `RAck` and a
// body: the whole family reads those, and the `Content-Length` is what a
// rule asks about a body from a head-only vantage.

const SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                       m=audio 5004 RTP/AVP 0\r\n";

/// The body bytes those rows describe — the live adapter parses the whole
/// datagram, so a declared length with nothing behind it is not a message.
fn body(sdp: bool) -> &'static str {
    if sdp {
        SDP
    } else {
        ""
    }
}

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

/// A request carrying an optional SDP body.
fn req_sdp(
    method: &str,
    branch: &str,
    cseq: u32,
    ttag: Option<&str>,
    body: Option<&str>,
) -> Vec<u8> {
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    let body = body.unwrap_or("");
    let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let mut v = format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    v.extend_from_slice(body.as_bytes());
    v
}

/// A response carrying an optional SDP body.
fn resp_sdp(status: u16, cseq: u32, method: &str, branch: &str, body: Option<&str>) -> Vec<u8> {
    let body = body.unwrap_or("");
    let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let mut v = format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    v.extend_from_slice(body.as_bytes());
    v
}

/// An arrival of `raw` at `bind` with the given disposition and §17.2
/// repeat mark, as the recording stamps it at production.
fn arrival(
    bind: &str,
    raw: Vec<u8>,
    src: &str,
    seq: u64,
    disposition: crate::types::RecvDisposition,
    repeat_of: Option<u64>,
) -> Stamped<SignalingNetworkEvent> {
    let mut wire = crate::contracts::WireStamp::of_bytes(&raw);
    wire.repeat_of = repeat_of;
    Stamped {
        event: SignalingNetworkEvent::RecvItem {
            bind_key: bind.to_string(),
            disposition,
            wire,
            packet: UdpPacket::unparsed(raw, src.parse().unwrap(), seq),
        },
        seq,
        at_ms: seq,
    }
}

/// The audit view's repeat mark is relative to what the view holds: when
/// the loss model dropped the first copy of an offer, the retransmission is
/// the first copy the endpoint took, so its offer stands and a bodyless ACK
/// to the answering 2xx is clean. A later copy is still a repeat.
#[test]
fn a_retransmission_of_a_lost_arrival_is_the_first_copy_the_view_holds() {
    use crate::types::RecvDisposition::{Delivered, LossModel};
    let invite = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
    let evs = vec![
        arrival(BOB, invite.clone(), ALICE, 0, LossModel, None),
        arrival(BOB, invite.clone(), ALICE, 1, Delivered, Some(0)),
        arrival(BOB, invite, ALICE, 2, Delivered, Some(0)),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 3),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 4),
    ];
    let visible: Vec<_> =
        evs.iter().filter(|s| crate::contracts::audit_visible_event(&s.event)).cloned().collect();
    let views = bind_views(&visible);
    let repeats: Vec<bool> = views[0].msgs.iter().map(|m| m.repeat).collect();
    assert_eq!(repeats, vec![false, true, false, false], "only the second copy repeats");
    let findings = evaluate_rfc_findings(&evs);
    assert!(
        !findings.iter().any(|f| f.rule == rfc_rules::RuleId::DelayedOfferAnsweredInAck.token()),
        "{findings:?}",
    );
}
