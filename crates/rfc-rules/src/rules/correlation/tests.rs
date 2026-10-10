//! The family's OWN semantics: which message is an occasion, what the
//! taker's own stream settles, and what a relaying vantage may not judge.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::*;

mod cancel_target;
mod dialog_id;
mod from_tag;
mod to_tag;
mod transaction;

const A: &str = "10.0.0.1:5060";
const B: &str = "10.0.0.2:5070";

#[allow(clippy::too_many_arguments)]
fn msg(
    at_us: u64,
    src: &str,
    dst: &str,
    kind: Kind,
    call_id: &str,
    cseq: u32,
    cseq_method: &str,
    branch: Option<&str>,
    from_tag: Option<&str>,
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
        call_id: call_id.to_string(),
        cseq,
        cseq_method: cseq_method.to_string(),
        via_branch: branch.map(str::to_string),
        from_tag: from_tag.map(str::to_string),
        to_tag: to_tag.map(str::to_string),
        head: Some(head.into_bytes()),
        body: Some(Vec::new()),
    }
}

/// A request `src` sent, with `vias` Via rows.
#[allow(clippy::too_many_arguments)]
fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    uri: &str,
    cseq: u32,
    branch: &str,
    from_uri: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    vias: usize,
) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@h>;tag={t}"),
        None => "<sip:bob@h>".to_string(),
    };
    let mut head = format!("{method} {uri} SIP/2.0\r\n");
    for i in 0..vias.max(1) {
        let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
        head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
    }
    head.push_str(&format!(
        "From: <{from_uri}>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
    ));
    msg(
        at_us,
        src,
        dst,
        Kind::Request { method: method.to_string() },
        "c1",
        cseq,
        method,
        Some(branch),
        Some(from_tag),
        to_tag,
        head,
    )
}

/// A response `src` sent, with `vias` Via rows.
#[allow(clippy::too_many_arguments)]
fn resp(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    method: &str,
    branch: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    vias: usize,
) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@h>;tag={t}"),
        None => "<sip:bob@h>".to_string(),
    };
    let mut head = format!("SIP/2.0 {status} X\r\n");
    for i in 0..vias {
        let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
        head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
    }
    head.push_str(&format!(
        "From: <sip:alice@h>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
    ));
    msg(
        at_us,
        src,
        dst,
        Kind::Response { status },
        "c1",
        cseq,
        method,
        Some(branch),
        Some(from_tag),
        to_tag,
        head,
    )
}

fn obs(msgs: &[Msg]) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed: true }
}

fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// Only the findings charged to `emitter` — the live consumer's vantage
/// filter, applied here so a test states one party's verdict.
fn charged(f: Vec<Finding>, emitter: &str) -> Vec<Finding> {
    f.into_iter().filter(|x| x.emitter == emitter).collect()
}

/// A dialog A opens to B: INVITE, 180, 200, and the tags they mint.
fn opened() -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
    ]
}
