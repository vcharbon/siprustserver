//! One process reads the wire one way. Through the runner's own wiring, with a
//! limit no default states, a datagram reads the same whichever stage parses
//! it: the transaction layer below the brake's threshold, the brake at it, and
//! the brake below it while a refused INVITE is remembered. Its own target: it
//! sets the process environment.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_runner_kit::{ConfigCheck, RunnerEnv};
use sip_message::{CustomParser, SipMessage, SipParser};
use tokio::net::UdpSocket;

/// The most lines the test's process reads.
const LIMIT: usize = 12;

/// An OPTIONS of `lines` lines in all, from a peer at `port`.
fn options(lines: usize, id: &str, port: u16) -> Vec<u8> {
    let own = [
        "OPTIONS sip:worker@127.0.0.1 SIP/2.0".to_string(),
        format!("Via: SIP/2.0/UDP 127.0.0.1:{port};branch=z9hG4bK-{id}"),
        "Max-Forwards: 70".to_string(),
        "From: <sip:alice@127.0.0.1>;tag=a1".to_string(),
        "To: <sip:worker@127.0.0.1>".to_string(),
        format!("Call-ID: {id}@127.0.0.1"),
        "CSeq: 1 OPTIONS".to_string(),
        "Content-Length: 0".to_string(),
    ];
    let pad = std::iter::repeat_n("X-Pad: p".to_string(), lines - own.len());
    let all: Vec<String> = own.into_iter().chain(pad).collect();
    format!("{}\r\n\r\n", all.join("\r\n")).into_bytes()
}

/// A worker through the runner's wiring reading under [`LIMIT`], `tune` applied
/// to its environment, its refusals handed to `prepare` before the first datagram.
async fn worker(
    tune: impl FnOnce(&mut RunnerEnv),
    prepare: impl FnOnce(&b2bua::admission::Refusals),
) -> (b2bua_runner_kit::RunnerBase, Arc<b2bua::B2buaCore>) {
    let mut env = RunnerEnv::from_env().expect("the environment reads");
    env.parser_limits.max_header_lines = LIMIT;
    tune(&mut env);
    let mut base = env.bind("parser-wiring").await.expect("bound");
    prepare(&base.refusals);
    // The brake's threshold is cut from the queue bound at bind; the spawn's
    // check reads the stated bound.
    base.env.queue_max = base.env.queue_max.max(256);
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 9));
    let deps = base.deps(decision, None).await;
    let core = base.spawn(deps, Vec::new(), ConfigCheck::new()).expect("spawned");
    (base, core)
}

/// What the worker answers an OPTIONS of `lines` lines with.
async fn answer(worker: std::net::SocketAddr, lines: usize, id: &str) -> Option<Vec<u8>> {
    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("the peer");
    let port = peer.local_addr().expect("its address").port();
    peer.send_to(&options(lines, id, port), worker).await.expect("sent");
    let mut buf = vec![0u8; 65_535];
    let got = tokio::time::timeout(Duration::from_millis(150), peer.recv_from(&mut buf)).await;
    got.ok().map(|got| {
        let (n, _) = got.expect("received");
        buf.truncate(n);
        buf
    })
}

/// The same pair of datagrams through `base`: at the limit it answers, past it
/// nothing answers.
async fn reads_the_same(stage: &str, base: &b2bua_runner_kit::RunnerBase) {
    let within = answer(base.local, LIMIT, "within").await;
    let within = within.unwrap_or_else(|| panic!("{stage}: a datagram at the limit is answered"));
    let reply = CustomParser::new().parse(&within).expect("a response");
    assert!(matches!(reply, SipMessage::Response(_)), "{stage}: {reply:?}");

    let past = answer(base.local, LIMIT + 1, "past").await;
    assert!(past.is_none(), "{stage}: past the limit nothing answers: {past:?}");
}

#[tokio::test]
async fn a_datagram_reads_the_same_whichever_stage_parses_it() {
    // SAFETY: the one test of this target, before any thread reads the environment.
    for (key, value) in [
        ("B2BUA_LISTEN", "127.0.0.1:0"),
        ("B2BUA_METRICS", "127.0.0.1:0"),
        ("B2BUA_ADVERTISE", "127.0.0.1:5060"),
        ("B2BUA_ADMIN", "127.0.0.1:0"),
        ("B2BUA_MODE", "standalone"),
    ] {
        std::env::set_var(key, value);
    }

    let (below, _core) = worker(|env| env.udp_ingress_brake_pct = 100, |_| {}).await;
    reads_the_same("below the threshold", &below).await;

    let (above, _core) = worker(
        |env| {
            // A threshold of no datagrams: the brake parses each one.
            env.queue_max = 1;
            env.udp_ingress_brake_pct = 1;
        },
        |_| {},
    )
    .await;
    reads_the_same("at the threshold", &above).await;

    let refused_invite = b"INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:9;branch=z9hG4bK-refused\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@127.0.0.1>;tag=a1\r\n\
        To: <sip:bob@127.0.0.1>\r\n\
        Call-ID: refused@127.0.0.1\r\n\
        CSeq: 1 INVITE\r\n\
        Contact: <sip:alice@127.0.0.1:9>\r\n\
        Content-Length: 0\r\n\r\n";
    let (memo, _core) = worker(
        |env| env.udp_ingress_brake_pct = 100,
        |refusals| {
            let SipMessage::Request(invite) =
                CustomParser::new().parse(refused_invite).expect("the INVITE reads")
            else {
                panic!("a request");
            };
            let _ = refusals.memo().refuse(&invite);
            assert!(refusals.memo().remembers());
        },
    )
    .await;
    reads_the_same("under a live memo", &memo).await;
}
