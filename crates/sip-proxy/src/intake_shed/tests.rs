//! [`intake_shed_hook`] and [`intake_shed_watermark`], each case invoking the
//! hook directly over a datagram and a queue depth, with no socket.

use super::*;
use sip_message::Bytes;
use sip_net::{Arrival, QueueLoad};
use std::net::SocketAddr;

use crate::addr::ProxyAddr;
use crate::observability::ProxyMetrics;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerEntry;

const WATERMARK: usize = 8;
/// The proxy's internal (or only) face.
const PROXY: (&str, u16) = ("192.0.2.1", 5060);
/// The registry's one worker; no datagram below is sent by it unless a case
/// says so.
const WORKER: (&str, u16) = ("10.0.0.2", 5070);

fn src() -> SocketAddr {
    "192.0.2.9:5060".parse().unwrap()
}

/// The internal face of a single-face proxy on `PROXY` whose registry lists
/// `WORKER`.
fn internal() -> IntakeFace {
    let registry = StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
        "w1",
        ProxyAddr::new(WORKER.0, WORKER.1),
    )]);
    IntakeFace::Internal(WorkerSigns::new(
        Arc::new(registry),
        ProxyAddr::new(PROXY.0, PROXY.1),
        None,
    ))
}

fn hook_on(face: IntakeFace) -> (PreIngressHook, Arc<ProxyMetrics>) {
    let metrics = Arc::new(ProxyMetrics::new());
    (intake_shed_hook(WATERMARK, metrics.clone(), CustomParser::new(), face), metrics)
}

fn shed_hook() -> (PreIngressHook, Arc<ProxyMetrics>) {
    hook_on(internal())
}

fn judge(hook: &PreIngressHook, raw: &[u8], depth: usize) -> PreIngressAction {
    judge_from(hook, raw, src(), depth)
}

fn judge_from(
    hook: &PreIngressHook,
    raw: &[u8],
    from: SocketAddr,
    depth: usize,
) -> PreIngressAction {
    hook(&mut Arrival::new(Bytes::copy_from_slice(raw)), from, QueueLoad::at_depth(depth))
}

/// A new INVITE whose top Via names `sent_by`, with `extra` header lines.
fn new_invite_via(sent_by: &str, extra: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@198.51.100.7:5060 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP {sent_by};branch=z9hG4bK-signs\r\n\
{extra}Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: signs@192.0.2.10\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn invite_with_to(to: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@192.0.2.20:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-shape\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
{to}\r\n\
Call-ID: shape@192.0.2.10\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

const NEW_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const IN_DIALOG_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-re\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n";

const EMERGENCY_INVITE: &[u8] = b"INVITE sip:911@10.0.0.2:5070 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-em\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:911@example.com>\r\n\
Resource-Priority: esnet.0\r\n\
Call-ID: em@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const BYE: &[u8] = b"BYE sip:bob@10.0.0.2:5070 SIP/2.0\r\nMax-Forwards: 70\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bye\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 3 BYE\r\nContent-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\nContact: <sip:fixture@192.0.2.98>\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

/// Bytes the parser refuses with nothing a 400 could echo.
const GARBAGE: &[u8] = b"\x00 not SIP";

/// A new INVITE the parser refuses on a gate (a non-numeric Expires) with
/// every field a 400 echoes intact.
const ANSWERABLE: &[u8] =
    b"INVITE sip:bob@192.0.2.20:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-answerable\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: answerable@192.0.2.10\r\nCSeq: 1 INVITE\r\nExpires: x\r\nContent-Length: 0\r\n\r\n";

#[test]
fn below_the_watermark_everything_is_admitted() {
    let (hook, _) = shed_hook();
    for raw in [NEW_INVITE, IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(judge(&hook, raw, WATERMARK - 1), PreIngressAction::Accept);
    }
}

/// The hook parses every datagram it is given, below the watermark too, and
/// leaves the parse on the arrival for the core; below the watermark a
/// datagram the parser refuses is queued with its refusal, which the core
/// answers or drops.
#[test]
fn every_datagram_is_parsed_on_the_receive_path() {
    let (hook, _) = shed_hook();
    for depth in [0, WATERMARK] {
        for raw in [IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
            let mut arrival = Arrival::new(Bytes::from_static(raw));
            let load = QueueLoad::at_depth(depth);
            assert_eq!(hook(&mut arrival, src(), load), PreIngressAction::Accept);
            assert!(matches!(arrival.parsed(), Some(Ok(Ok(_)))), "{depth}: {:?}", arrival.parsed());
        }
    }
    let mut garbage = Arrival::new(Bytes::from_static(GARBAGE));
    assert_eq!(hook(&mut garbage, src(), QueueLoad::at_depth(0)), PreIngressAction::Accept);
    assert!(matches!(garbage.parsed(), Some(Ok(Err(_)))), "{:?}", garbage.parsed());
}

#[test]
fn at_the_watermark_only_the_new_non_emergency_invite_is_shed() {
    let (hook, metrics) = shed_hook();
    assert_eq!(judge(&hook, NEW_INVITE, WATERMARK), PreIngressAction::Drop);
    for admitted in [IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(judge(&hook, admitted, WATERMARK), PreIngressAction::Accept);
    }
    assert_eq!((metrics.intake_shed(), metrics.intake_unanswerable()), (1, 0));
}

/// At the watermark a datagram no response can answer (ADR-0054 §4) is
/// dropped at intake and counted apart from the shed, and as the parse error
/// the core would have counted; a refusal a 400 can answer is queued for the
/// core's capped answer. Below the watermark both are queued.
#[test]
fn at_the_watermark_an_unanswerable_datagram_is_dropped_and_an_answerable_one_queued() {
    let (hook, metrics) = shed_hook();
    assert_eq!(judge(&hook, GARBAGE, WATERMARK - 1), PreIngressAction::Accept);
    assert_eq!(judge(&hook, ANSWERABLE, WATERMARK - 1), PreIngressAction::Accept);
    assert_eq!(judge(&hook, GARBAGE, WATERMARK), PreIngressAction::Drop);
    assert_eq!(judge(&hook, ANSWERABLE, WATERMARK), PreIngressAction::Accept);
    assert_eq!(metrics.intake_unanswerable(), 1);
    assert_eq!(metrics.parse_errors(), 1);
    assert_eq!(metrics.intake_shed(), 0);
}

/// The shed reads the To-tag exactly as the parser does (RFC 3261 §20.39,
/// §25.1): a `tag=` inside the display name or the URI is no tag, and a tag
/// parameter written with linear whitespace around `;` and `=`, or folded
/// onto a continuation line, is one.
#[test]
fn the_shed_reads_the_to_tag_as_the_parser_does() {
    use sip_message::{CustomParser, SipMessage, SipParser};
    let (hook, _) = shed_hook();
    let mut misread = Vec::new();
    for (to, tag, verdict) in [
        ("To: \"tag=FAKE\" <sip:bob@example.com>", None, PreIngressAction::Drop),
        ("To: <sip:bob@example.com;tag=URI>", None, PreIngressAction::Drop),
        ("To: <sip:bob@example.com>; tag = REAL", Some("REAL"), PreIngressAction::Accept),
        ("To: <sip:bob@example.com>\r\n ;tag=REAL", Some("REAL"), PreIngressAction::Accept),
    ] {
        let raw = invite_with_to(to);
        let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&raw) else {
            panic!("the parser reads {to:?}");
        };
        assert_eq!(req.to().tag(), tag, "the parser's To-tag of {to:?}");
        let judged = judge(&hook, &raw, WATERMARK);
        if judged != verdict {
            misread.push(format!("{to:?}: {judged:?}, not {verdict:?}"));
        }
    }
    assert!(misread.is_empty(), "the shed misreads the To-tag of {misread:#?}");
}

/// At the watermark the internal face queues a new INVITE carrying any one
/// worker sign, as the core reads it: a source that is a registry worker, a
/// top Via sent-by that is one, or `;outbound` on the direction Route entry
/// naming this proxy. A marker on a Route naming another host, or this
/// proxy's Route without the marker, is no sign. The external face sheds
/// every one of them.
#[test]
fn at_the_watermark_the_internal_face_queues_a_new_invite_with_a_worker_sign() {
    let worker: SocketAddr = format!("{}:{}", WORKER.0, WORKER.1).parse().unwrap();
    let (worker_via, other_via) = (format!("{}:{}", WORKER.0, WORKER.1), "192.0.2.10:5060");
    let own_marker = format!("Route: <sip:{}:{};lr;outbound>\r\n", PROXY.0, PROXY.1);
    let own_plain = format!("Route: <sip:{}:{};lr>\r\n", PROXY.0, PROXY.1);
    let foreign_marker = "Route: <sip:198.51.100.9:5060;lr;outbound>\r\n";
    let cases = [
        ("worker source", worker, new_invite_via(other_via, ""), PreIngressAction::Accept),
        ("worker Via", src(), new_invite_via(&worker_via, ""), PreIngressAction::Accept),
        ("own marker", src(), new_invite_via(other_via, &own_marker), PreIngressAction::Accept),
        ("own plain Route", src(), new_invite_via(other_via, &own_plain), PreIngressAction::Drop),
        (
            "foreign marker",
            src(),
            new_invite_via(other_via, foreign_marker),
            PreIngressAction::Drop,
        ),
    ];
    let (internal, internal_metrics) = hook_on(internal());
    let (external, external_metrics) = hook_on(IntakeFace::External);
    for (what, from, raw, verdict) in &cases {
        assert_eq!(judge_from(&internal, raw, *from, WATERMARK), *verdict, "internal: {what}");
        assert_eq!(judge_from(&external, raw, *from, WATERMARK), PreIngressAction::Drop, "{what}");
    }
    assert_eq!(internal_metrics.intake_shed(), 2);
    assert_eq!(external_metrics.intake_shed(), 5);
}

#[test]
fn deeper_than_the_watermark_still_sheds_new_invites() {
    let (hook, _) = shed_hook();
    assert_eq!(judge(&hook, NEW_INVITE, WATERMARK * 100), PreIngressAction::Drop);
}

#[test]
fn watermark_is_the_percentage_of_the_queue_and_floors_at_one() {
    assert_eq!(intake_shed_watermark(8192, 50), 4096);
    assert_eq!(intake_shed_watermark(1, 50), 1);
    assert_eq!(intake_shed_watermark(8192, 0), 1);
}
