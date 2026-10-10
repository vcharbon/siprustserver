//! The family's OWN semantics: what an adapter's event fixtures cannot
//! state — the closed-observation ruling (D2), the fork partition, and
//! each rule's undecidable gates.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    Delay2xxOnUnackedReliable1xxWithSdp, NoNewReliable1xxAfterFinal,
    NoOverlappingReliableProvisionals, NoPrackOf100Trying, NoPrackOfOutOfOrderRseq,
    NoReliable1xxOnInDialog, NonContiguousRseq, Prack2xxOr481, PrackAnswers1xxOffer,
    RackWithoutKnownInvite, ReliableNeedsClientOptIn, RequireReliable1xxOnRequire,
    UnackedReliableProvisional, UnmatchedPrackProxied,
};

mod acknowledgement;
mod answering;
mod negotiation;
mod offer;
mod sequence;

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";

fn msg(
    at_us: u64,
    src: &str,
    dst: &str,
    kind: Kind,
    cseq: u32,
    to_tag: Option<&str>,
    head: String,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind,
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        via_branch: None,
        from_tag: Some("fa".to_string()),
        to_tag: to_tag.map(str::to_string),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// An INVITE offering `100rel`, from `src`.
fn invite(at_us: u64, src: &str, dst: &str, cseq: u32) -> Msg {
    let head = format!(
        "INVITE sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>\r\n\
             CSeq: {cseq} INVITE\r\nSupported: 100rel\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, src, dst, Kind::Request { method: "INVITE".to_string() }, cseq, None, head)
}

/// A reliable provisional the UAS sends on early dialog `to_tag`.
fn reliable(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    rseq: u64,
    to_tag: &str,
) -> Msg {
    let head = format!(
        "SIP/2.0 {status} Ringing\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nRequire: 100rel\r\nRSeq: {rseq}\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, src, dst, Kind::Response { status }, cseq, Some(to_tag), head)
}

/// A PRACK whose `RAck` names `(rseq, rack_cseq)`.
fn prack(
    at_us: u64,
    src: &str,
    dst: &str,
    cseq: u32,
    rseq: u64,
    rack_cseq: u32,
    to_tag: &str,
) -> Msg {
    let head = format!(
        "PRACK sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} PRACK\r\nRAck: {rseq} {rack_cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m = msg(
        at_us,
        src,
        dst,
        Kind::Request { method: "PRACK".to_string() },
        cseq,
        Some(to_tag),
        head,
    );
    m.cseq_method = "PRACK".to_string();
    m
}

/// The INVITE final that releases the transaction.
fn ok200(at_us: u64, cseq: u32, to_tag: &str) -> Msg {
    let head = format!(
        "SIP/2.0 200 OK\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, UAS, UAC, Kind::Response { status: 200 }, cseq, Some(to_tag), head)
}

fn obs(msgs: &[Msg], closed: bool) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed }
}

// ── the §3 negotiation half ─────────────────────────────────────────────
//
// These rules pair a request an endpoint TOOK on a top-Via branch with the
// responses it SENT there, so their fixtures carry a branch (the ones above
// deliberately do not — the obligation half never keys on one).

const INV_BRANCH: &str = "z9hG4bK-i";
const PRACK_BRANCH: &str = "z9hG4bK-p";
const UPDATE_BRANCH: &str = "z9hG4bK-u";
const SDP: &str = "v=0\r\no=- 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";

/// The body rows a message carries, as a rule reads them off the head.
fn body_rows(body: bool) -> String {
    match body {
        true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
        false => "Content-Length: 0\r\n".to_string(),
    }
}

/// The reliable-provisional markers §3 requires of BOTH halves.
fn reliable_rows(rseq: u64) -> String {
    format!("Require: 100rel\r\nRSeq: {rseq}\r\n")
}

/// An INVITE the UAS takes, with caller-chosen option-tag rows, To tag and
/// body.
fn invite_on(at_us: u64, extra: &str, to_tag: Option<&str>, body: bool) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:b@h>;tag={t}"),
        None => "<sip:b@h>".to_string(),
    };
    let head = format!(
        "INVITE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: {to}\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
        body_rows(body)
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "INVITE".to_string() }, 1, to_tag, head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// An INVITE-transaction response the UAS sends.
fn inv_resp(at_us: u64, status: u16, extra: &str, body: bool) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
        body_rows(body)
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some("tb"), head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// A PRACK the UAC sends, its `RAck` spelled out.
fn prack_on(at_us: u64, rack: &str, body: bool) -> Msg {
    let head = format!(
        "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n{}\r\n",
        body_rows(body)
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "PRACK".to_string() }, 2, Some("tb"), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(PRACK_BRANCH.to_string());
    m
}

/// The UAS's response on that PRACK's own server transaction.
fn prack_answer(at_us: u64, status: u16) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(PRACK_BRANCH.to_string());
    m
}

fn decide(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs, true) })
}

/// A reliable provisional on a chosen early dialog — what a fork looks like
/// on the branch it shares with its sibling.
fn fork_resp(at_us: u64, status: u16, extra: &str, to_tag: &str, sdp: bool) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 1 INVITE\r\n\
             {extra}{}\r\n",
        body_rows(sdp)
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some(to_tag), head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// The PRACK of one fork's provisional, on that fork's own dialog.
fn fork_prack(at_us: u64, rack: &str, to_tag: &str, branch: &str) -> Msg {
    let head = format!(
        "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "PRACK".to_string() }, 2, Some(to_tag), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(branch.to_string());
    m
}
