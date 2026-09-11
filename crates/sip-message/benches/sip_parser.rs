//! SIP-stack micro-benchmarks, reported as **elements/sec = SIP messages/sec**
//! (criterion `Throughput::Elements(1)`).
//!
//! Five groups, on the cases and fixtures `tests/alloc_budget.rs` budgets:
//!
//! - `decode/*` — parse only (raw bytes → `SipMessage`).
//! - `decode_shared/*` — the receive path, where the caller owns the datagram
//!   and the message shares that buffer.
//! - `hop/*` — the thawed-draft edit alone, on a message parsed outside the
//!   timed region: the full proxy rewrite set (RFC 3261 §16.4/§16.6) and the
//!   received/rport stamp.
//! - `proxy_hop/*` — decode plus one minimal hop: the per-inbound-datagram
//!   SIP-stack cost.
//! - `build/*` — origination: the blank draft driven directly, and the
//!   generator recipes over stringly options.
//!
//! `proxy_hop` is the SIP-stack ceiling: it excludes routing policy,
//! transaction state, sockets and the HTTP decision call — so real proxy
//! throughput is at or below these numbers.
//!
//! Run: `cargo bench -p sip-message`

#[path = "../tests/perf/mod.rs"]
mod perf;

use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sip_message::generators::{
    generate_in_dialog_request, generate_out_of_dialog_request, generate_response, InDialogMethod,
    OutOfDialogMethod,
};
use sip_message::{CustomParser, SipParser};

use perf::BlankInvite;

fn bench_decode(c: &mut Criterion) {
    let parser = CustomParser::new();
    let invite_sdp = perf::invite_with_sdp();
    let mut group = c.benchmark_group("decode");
    group.throughput(Throughput::Elements(1));
    group.bench_function("invite", |b| b.iter(|| parser.parse(black_box(perf::INVITE)).unwrap()));
    group.bench_function("invite_sdp", |b| {
        b.iter(|| parser.parse(black_box(invite_sdp.as_slice())).unwrap())
    });
    group.bench_function("200_ok", |b| b.iter(|| parser.parse(black_box(perf::OK_200)).unwrap()));
    group.finish();

    // The receive path: the caller owns the datagram and hands it over, so the
    // message shares that buffer instead of copying it. `Bytes::clone` here is
    // a refcount bump standing in for the socket's per-datagram buffer.
    let shared_invite = Bytes::from_static(perf::INVITE);
    let shared_sdp = Bytes::from(invite_sdp);
    let mut group = c.benchmark_group("decode_shared");
    group.throughput(Throughput::Elements(1));
    group.bench_function("invite", |b| {
        b.iter(|| parser.parse_shared(black_box(shared_invite.clone())).unwrap())
    });
    group.bench_function("invite_sdp", |b| {
        b.iter(|| parser.parse_shared(black_box(shared_sdp.clone())).unwrap())
    });
    group.finish();
}

fn bench_hop(c: &mut Criterion) {
    let parser = CustomParser::new();
    let in_dialog = perf::in_dialog_invite_with_sdp();
    let inbound = perf::request(&parser, &in_dialog);

    let mut group = c.benchmark_group("hop");
    group.throughput(Throughput::Elements(1));
    group.bench_function("rewrite_set", |b| {
        b.iter(|| black_box(perf::hop_rewrite_set(black_box(&inbound))))
    });
    group.bench_function("stamp_rport", |b| {
        b.iter(|| black_box(perf::hop_stamp_received_rport(black_box(&inbound))))
    });
    group.finish();

    let invite_sdp = perf::invite_with_sdp();
    let mut group = c.benchmark_group("proxy_hop");
    group.throughput(Throughput::Elements(1));
    group.bench_function("invite", |b| {
        b.iter(|| black_box(perf::hop_minimal(&perf::request(&parser, black_box(perf::INVITE)))))
    });
    group.bench_function("invite_sdp", |b| {
        b.iter(|| {
            black_box(perf::hop_minimal(&perf::request(&parser, black_box(invite_sdp.as_slice()))))
        })
    });
    group.finish();
}

fn bench_build(c: &mut Criterion) {
    let parser = CustomParser::new();
    let invite_sdp = perf::invite_with_sdp();
    let sdp = perf::body_of(&invite_sdp);
    let blank = BlankInvite::new(&sdp);
    let dialog = perf::build_dialog();
    let invite_opts = perf::invite_opts(&sdp);
    let bye_opts = perf::bye_opts();
    let response_opts = perf::response_opts();
    let parsed_invite = perf::request(&parser, perf::INVITE);

    let mut group = c.benchmark_group("build");
    group.throughput(Throughput::Elements(1));
    group.bench_function("blank_draft", |b| b.iter(|| black_box(blank.build())));
    group.bench_function("invite_sdp", |b| {
        b.iter(|| {
            black_box(generate_out_of_dialog_request(
                OutOfDialogMethod::Invite,
                black_box(&invite_opts),
            ))
        })
    });
    group.bench_function("bye", |b| {
        b.iter(|| {
            black_box(generate_in_dialog_request(
                InDialogMethod::Bye,
                &dialog,
                black_box(&bye_opts),
            ))
        })
    });
    group.bench_function("response_200", |b| {
        b.iter(|| {
            black_box(generate_response(&parsed_invite, 200, "OK", black_box(&response_opts)))
        })
    });
    group.finish();
}

criterion_group!(benches, bench_decode, bench_hop, bench_build);
criterion_main!(benches);
