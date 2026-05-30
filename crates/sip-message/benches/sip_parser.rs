//! Parse-throughput micro-benchmark. Port of `bench/sip-parser-bench.ts`.
//! Mirrors the ADR-0007 throughput table (INVITE / 200 OK / BYE corpora).
//!
//! Run: `cargo bench -p sip-message`
//!
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use sip_message::{CustomParser, SipParser};

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

fn bench_parse(c: &mut Criterion) {
    let parser = CustomParser::new();
    let mut group = c.benchmark_group("parse");
    group.bench_function("invite", |b| b.iter(|| parser.parse(black_box(INVITE)).unwrap()));
    group.bench_function("200_ok", |b| b.iter(|| parser.parse(black_box(OK_200)).unwrap()));
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
