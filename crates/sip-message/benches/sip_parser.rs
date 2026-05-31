//! SIP-stack micro-benchmarks. Port of `bench/sip-parser-bench.ts`, extended
//! with a proxy hot-path bench.
//!
//! Two measurements, both reported as **elements/sec = SIP messages/sec**
//! (criterion `Throughput::Elements(1)`):
//!
//!   1. `decode/*`     — parse only (raw bytes → `SipMessage`).
//!   2. `proxy_hop/*`  — the full per-message SIP-stack cost a B2BUA/proxy pays
//!                       on the forwarding path: decode → clone (work on a copy,
//!                       leaving the inbound message intact) → rewrite the
//!                       Request-URI → insert a Record-Route header
//!                       (RFC 3261 §16.6) → encode back to wire bytes.
//!
//! `proxy_hop` is the SIP-stack ceiling: it excludes routing-policy, transaction
//! state, sockets and the HTTP decision call — so the real proxy throughput is
//! at or below these numbers. A real proxy also stamps its own Via and
//! decrements Max-Forwards; those are O(1) string ops in the same noise band as
//! the Record-Route insert and are omitted to match the requested shape.
//!
//! Run: `cargo bench -p sip-message`

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sip_message::{serialize, CustomParser, SipHeader, SipMessage, SipParser};

const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:bob@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

/// A realistic INVITE carrying an SDP offer (what a proxy actually forwards) —
/// built at runtime so Content-Length matches the body exactly.
fn invite_with_sdp() -> Vec<u8> {
    let sdp = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
        sdp.len(),
        sdp
    )
    .into_bytes()
}

fn bench_decode(c: &mut Criterion) {
    let parser = CustomParser::new();
    let invite_sdp = invite_with_sdp();
    let mut group = c.benchmark_group("decode");
    group.throughput(Throughput::Elements(1));
    group.bench_function("invite", |b| b.iter(|| parser.parse(black_box(INVITE)).unwrap()));
    group.bench_function("invite_sdp", |b| b.iter(|| parser.parse(black_box(invite_sdp.as_slice())).unwrap()));
    group.bench_function("200_ok", |b| b.iter(|| parser.parse(black_box(OK_200)).unwrap()));
    group.finish();
}

/// One proxy forwarding hop: decode → clone → rewrite R-URI → add Record-Route
/// → encode. Returns the re-serialized bytes so the optimizer can't elide it.
fn proxy_hop(parser: &CustomParser, raw: &[u8]) -> Vec<u8> {
    let msg = parser.parse(raw).expect("parse");
    let SipMessage::Request(req) = msg else { panic!("expected request") };

    // Work on a copy, leaving the inbound message intact (B2BUA two-leg model).
    let mut out = req.clone();

    // Rewrite the Request-URI to the next-hop target.
    out.uri = "sip:bob@192.0.2.99:5060".to_string();

    // Insert our Record-Route at the top of the header set (RFC 3261 §16.6).
    out.headers.insert(
        0,
        SipHeader { name: "Record-Route".to_string(), value: "<sip:proxy.example.com;lr>".to_string() },
    );

    serialize(&SipMessage::Request(out))
}

fn bench_proxy_hop(c: &mut Criterion) {
    let parser = CustomParser::new();
    let invite_sdp = invite_with_sdp();
    let mut group = c.benchmark_group("proxy_hop");
    group.throughput(Throughput::Elements(1));
    group.bench_function("invite", |b| b.iter(|| black_box(proxy_hop(&parser, black_box(INVITE)))));
    group.bench_function("invite_sdp", |b| {
        b.iter(|| black_box(proxy_hop(&parser, black_box(invite_sdp.as_slice()))))
    });
    group.finish();
}

criterion_group!(benches, bench_decode, bench_proxy_hop);
criterion_main!(benches);
