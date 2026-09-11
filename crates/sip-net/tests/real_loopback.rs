//! Real impl over loopback UDP — the behaviours TestClock can't model (real
//! sockets), the source's `it.live` tier. Each test binds ephemeral
//! `127.0.0.1:0` sockets and talks between them.

use std::sync::Arc;
use std::time::Duration;

use sip_net::types::PreIngressAction;
use sip_net::{BindUdpOpts, PreIngressHook, RealSignalingNetwork, SignalingNetwork};
use tokio::time::timeout;

fn loopback(queue_max: usize) -> BindUdpOpts {
    BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), queue_max)
}

#[tokio::test]
async fn loopback_send_recv() {
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64)).await.unwrap();

    a.send_to(b"hello over the wire", b.local_addr()).await.unwrap();

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw, b"hello over the wire");
    assert_eq!(pkt.src.ip(), a.local_addr().ip());
    assert_eq!(b.counters().enqueued, 1);
}

#[tokio::test]
async fn pre_ingress_reply_round_trips() {
    let net = RealSignalingNetwork::new();
    let hook: PreIngressHook = Arc::new(|raw: &[u8], _src, _depth| {
        if raw == b"PING" {
            PreIngressAction::Reply(b"PONG".to_vec())
        } else {
            PreIngressAction::Accept
        }
    });
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64).with_pre_ingress(hook)).await.unwrap();

    a.send_to(b"PING", b.local_addr()).await.unwrap();

    let reply = timeout(Duration::from_secs(2), a.recv()).await.expect("reply timed out").unwrap();
    assert_eq!(reply.raw, b"PONG");
    assert_eq!(b.counters().pre_ingress_replies, 1);
    assert_eq!(b.counters().enqueued, 0);
}

#[tokio::test]
async fn real_has_no_transit_or_inflight() {
    let net = RealSignalingNetwork::new();
    assert_eq!(net.transit_delay_ms(), None);
    assert_eq!(net.in_flight(), 0);
    assert!(net.queue_depths().is_empty());
    assert!(net.drain_undeliverable().await.is_empty());
}

/// SO_REUSEPORT sharding (Pass 9): two endpoints bind the SAME addr:port, and
/// every datagram of one flow (one src socket) lands on exactly ONE of them —
/// the kernel 4-tuple flow-hash that preserves per-flow ordering when the
/// proxy shards its recv loop.
#[tokio::test]
async fn reuse_port_shards_one_flow_to_one_socket() {
    let net = RealSignalingNetwork::new();
    // First bind picks the port (reuse_port set so the second can join it).
    let s1 = net.bind_udp(loopback(64).with_reuse_port(true)).await.expect("first reuse-port bind");
    let addr = s1.local_addr();
    let s2 = net
        .bind_udp(BindUdpOpts::new(addr, 64).with_reuse_port(true))
        .await
        .expect("second reuse-port bind on the same port");
    assert_eq!(s2.local_addr(), addr);

    // One flow: a single source socket sends N datagrams to the shared port.
    let uac = net.bind_udp(loopback(64)).await.unwrap();
    const N: usize = 20;
    for i in 0..N {
        uac.send_to(format!("pkt-{i}").as_bytes(), addr).await.unwrap();
    }

    // All N land on exactly one shard, in order. Wait until the kernel has
    // delivered all N (loopback — fast), then drain via try_recv.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while s1.counters().enqueued + s2.counters().enqueued < N as u64 {
        assert!(tokio::time::Instant::now() < deadline, "flow never fully arrived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (e1, e2) = (s1.counters().enqueued, s2.counters().enqueued);
    assert!(
        (e1 == N as u64 && e2 == 0) || (e2 == N as u64 && e1 == 0),
        "one flow must hash to exactly one shard (got s1={e1}, s2={e2})",
    );
    let receiver = if e1 > 0 { &s1 } else { &s2 };
    let got: Vec<usize> = std::iter::from_fn(|| receiver.try_recv())
        .map(|pkt| {
            std::str::from_utf8(&pkt.raw).unwrap().strip_prefix("pkt-").unwrap().parse().unwrap()
        })
        .collect();
    assert_eq!(got, (0..N).collect::<Vec<_>>(), "per-flow order preserved on one shard");
}

/// Without reuse_port, a second bind on a taken port still fails loudly — and
/// the error is structurally classified as addr-in-use (retryable), so a
/// bounded bind-retry loop never string-matches "Address already in use".
#[tokio::test]
async fn plain_rebind_still_conflicts() {
    let net = RealSignalingNetwork::new();
    let s1 = net.bind_udp(loopback(64)).await.unwrap();
    // Manual unwrap of the Err arm — `Box<dyn UdpEndpoint>` is not Debug.
    let err = match net.bind_udp(BindUdpOpts::new(s1.local_addr(), 64)).await {
        Ok(_) => panic!("non-reuse-port rebind must fail"),
        Err(e) => e,
    };
    assert!(err.is_addr_in_use(), "EADDRINUSE must classify as addr-in-use: {err:?}");
}

// ── UDP-only signalling: fragmentation, reception bound, error taxonomy ──────
//   (ADR-0027 — every socket fragments rather than refusing an oversize
//   datagram, and the receive buffer is bigger than any datagram can be.)

/// Every real bind pins the path-MTU-discovery mode to "never set DF", on both
/// address families. `IP_PMTUDISC_DO` is the mode that would fail an oversize
/// SIP message with EMSGSIZE — the pin exists to foreclose exactly that.
#[cfg(target_os = "linux")]
#[test]
fn every_bound_socket_pins_fragmentation_on_both_families() {
    use std::os::fd::AsRawFd;

    use sip_net::fragmentation::{mtu_discover_mode, PMTUDISC_DO, PMTUDISC_DONT};
    use sip_net::real::build_bound_socket;

    for (addr, ipv4) in [("127.0.0.1:0", true), ("[::1]:0", false)] {
        let socket = build_bound_socket(addr.parse().unwrap(), false).expect("bound socket");
        let mode = mtu_discover_mode(socket.as_raw_fd(), ipv4).expect("readable MTU-discover mode");
        assert_eq!(mode, PMTUDISC_DONT, "{addr} must be pinned to fragment");
        assert_ne!(mode, PMTUDISC_DO, "{addr} must never refuse an oversize datagram");
    }
}

/// A loopback address on this host whose path carries `len` bytes in ONE
/// datagram, probed with a plain socket before the stack is asked to do it.
/// `None` where no loopback can: a VM or container whose "loopback" is really a
/// 1 500-MTU device that drops IP fragments (WSL2 mirrored networking routes
/// `127.0.0.1` over exactly such a device). The size-dependent tests state that
/// skip rather than asserting a host property this stack does not own.
fn loopback_carrying(len: usize) -> Option<&'static str> {
    for addr in ["[::1]:0", "127.0.0.1:0"] {
        let (Ok(a), Ok(b)) = (std::net::UdpSocket::bind(addr), std::net::UdpSocket::bind(addr))
        else {
            continue;
        };
        let Ok(dst) = b.local_addr() else { continue };
        if b.set_read_timeout(Some(Duration::from_millis(500))).is_err() {
            continue;
        }
        if a.send_to(&vec![0u8; len], dst).is_err() {
            continue;
        }
        let mut buf = vec![0u8; 65_536];
        if matches!(b.recv_from(&mut buf), Ok((n, _)) if n == len) {
            return Some(addr);
        }
    }
    None
}

/// [`loopback`] on a given address.
fn loopback_on(addr: &str, queue_max: usize) -> BindUdpOpts {
    BindUdpOpts::new(addr.parse().unwrap(), queue_max)
}

/// A 9 000-byte datagram — over any Ethernet MTU — round-trips whole. NOTE: on
/// loopback (MTU 65 536) this does NOT prove IP fragmentation; it proves the
/// send path accepts a message far past the wire MTU and the receive path
/// hands it up intact. The fragmentation proof needs a real 1500-MTU link and
/// lives in the `#[ignore]`d slow lane below.
#[tokio::test]
async fn a_datagram_past_any_ethernet_mtu_round_trips_on_loopback() {
    let Some(addr) = loopback_carrying(9_000) else {
        eprintln!("SKIP: no loopback path on this host carries a 9000-byte datagram");
        return;
    };
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback_on(addr, 64)).await.unwrap();
    let b = net.bind_udp(loopback_on(addr, 64)).await.unwrap();

    let big = vec![b'x'; 9_000];
    a.send_to(&big, b.local_addr()).await.expect("a 9000-byte send is accepted");

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw.len(), 9_000, "the datagram arrives whole");
    assert_eq!(pkt.raw, big);
}

/// The largest datagram UDP can carry arrives with its exact length: the
/// receive buffer is strictly larger than `MAX_UDP_PAYLOAD`, so `recv_from`
/// never truncates a message into a torn one.
#[tokio::test]
async fn the_largest_possible_datagram_arrives_at_its_exact_length() {
    let Some(addr) = loopback_carrying(sip_net::MAX_UDP_PAYLOAD) else {
        eprintln!("SKIP: no loopback path on this host carries a max-size datagram");
        return;
    };
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback_on(addr, 64)).await.unwrap();
    let b = net.bind_udp(loopback_on(addr, 64)).await.unwrap();

    let max = vec![b'y'; sip_net::MAX_UDP_PAYLOAD];
    a.send_to(&max, b.local_addr()).await.expect("a max-size send is accepted");

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw.len(), sip_net::MAX_UDP_PAYLOAD, "no silent truncation");
}

/// A SIP INVITE the size of the largest one a real network carries (just under
/// 2 kB — past every 1 500-byte path) crosses the socket and still parses as
/// the message it was.
#[tokio::test]
async fn an_oversize_invite_crosses_the_socket_and_still_parses() {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    let Some(addr) = loopback_carrying(2_048) else {
        eprintln!("SKIP: no loopback path on this host carries a 2 kB datagram");
        return;
    };
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback_on(addr, 64)).await.unwrap();
    let b = net.bind_udp(loopback_on(addr, 64)).await.unwrap();

    let invite = oversize_invite(1_975);
    assert!(invite.len() >= 1_975, "the fixture must outgrow a 1500-byte path");
    a.send_to(invite.as_bytes(), b.local_addr()).await.expect("the INVITE is accepted");

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw.len(), invite.len(), "the INVITE arrives whole");
    let parsed = CustomParser::new().parse(&pkt.raw).expect("a readable INVITE");
    match parsed {
        SipMessage::Request(req) => assert_eq!(req.method().as_str(), "INVITE"),
        SipMessage::Response(_) => panic!("a request was sent"),
    }
}

/// A well-formed INVITE padded with extension header lines until it is at
/// least `at_least` bytes — the shape of a real oversize INVITE (long Route
/// sets, identity and charging headers, a big SDP), synthesized rather than
/// captured.
fn oversize_invite(at_least: usize) -> String {
    let mut msg = String::from(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK0123456789abcdef\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alice-tag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: oversize-invite@example.com\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@127.0.0.1:5060>\r\n\
         Content-Type: application/sdp\r\n",
    );
    let body = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
    let mut i = 0;
    while msg.len() + body.len() + 32 < at_least {
        msg.push_str(&format!("X-Padding-{i}: {}\r\n", "p".repeat(48)));
        i += 1;
    }
    msg.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    msg.push_str(body);
    msg
}

// ── Slow lane: fragmentation across a real 1 500-MTU link ───────────────────
//
// The default lane above cannot prove IP fragmentation: a loopback path either
// has a 65 536-byte MTU (nothing fragments) or drops fragments outright. These
// tests build a private network namespace whose loopback is pinned to MTU
// 1 500, then re-invoke this same test binary inside it — so the assertions run
// against the stack's own sockets, on a path that genuinely fragments.

/// Set on the re-invoked child so it runs the probe instead of setting up a
/// second namespace.
const CHILD_ENV: &str = "SIP_NET_FRAGMENTATION_CHILD";

/// What a child probe prints once its assertions have actually run.
const PROBE_RAN: &str = "fragmentation probe ran";

/// Run `child` (a test name in this binary) inside a fresh user+network
/// namespace whose loopback carries a 1 500-byte MTU, with `extra` shell lines
/// applied first. `None` when the namespace could not be built at all —
/// `unshare`/`ip` missing or refused — which is a host capability, not a
/// finding.
fn run_in_1500_mtu_netns(child: &str, extra: &str) -> Option<bool> {
    use std::process::Command;

    let exe = std::env::current_exe().ok()?;
    let script = format!(
        "set -e\n\
         ip link set lo up\n\
         ip link set lo mtu 1500\n\
         {extra}\n\
         exec \"$1\" --exact {child} --ignored --nocapture\n"
    );
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--", "bash", "-c", &script, "bash"])
        .arg(&exe)
        .env(CHILD_ENV, "1")
        .output()
        .ok()?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    // The namespace itself failed to come up (no unshare privileges, no `ip`,
    // no nft): not a verdict on the stack.
    if !out.status.success() && !text.contains("test result") {
        eprintln!("SKIP: could not build a 1500-MTU namespace: {text}");
        return None;
    }
    if !out.status.success() {
        eprintln!("{text}");
    }
    // A child that skipped its own body would still report success, so the
    // probe states that it ran and the parent demands that statement.
    assert!(text.contains(PROBE_RAN), "the child probe never ran inside the namespace: {text}");
    Some(out.status.success())
}

/// On a path that genuinely fragments, an oversize SIP datagram still leaves
/// and still arrives whole. The DISCRIMINATING assertion is the pair: the
/// socket is not in `IP_PMTUDISC_DO` (under which this send returns EMSGSIZE),
/// AND the 4 000-byte send succeeds. Either half alone proves nothing.
#[test]
#[ignore = "real-clock UDP in an unshare netns — slow lane (just test-slow); needs unshare"]
fn an_oversize_datagram_crosses_a_1500_mtu_link() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return; // the child runs `fragmentation_probe`
    }
    let Some(ok) = run_in_1500_mtu_netns("fragmentation_probe", "") else {
        return;
    };
    assert!(ok, "the oversize datagram must cross a 1500-MTU link");
}

/// Losing ONE non-first fragment loses the whole message: the receiver sees
/// nothing at all (never a truncated SIP message), which is what makes the
/// transaction layer's retransmission the recovery mechanism. Non-first
/// fragments carry no UDP ports, so the drop matches on the fragment offset —
/// a port match cannot select them.
#[test]
#[ignore = "real-clock UDP in an unshare netns — slow lane (just test-slow); needs unshare + nft"]
fn a_dropped_non_first_fragment_loses_the_whole_message() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return; // the child runs `fragment_loss_probe`
    }
    // Priority `raw` (-300): netfilter reassembles before the filter hooks, so
    // a rule any later would see one whole datagram and match nothing.
    let extra = "nft add table ip fragloss\n\
                 nft add chain ip fragloss pre '{ type filter hook prerouting priority -300; }'\n\
                 nft add rule ip fragloss pre ip frag-off '&' 0x1fff != 0 drop\n";
    let Some(ok) = run_in_1500_mtu_netns("fragment_loss_probe", extra) else {
        return;
    };
    assert!(ok, "a lost non-first fragment must lose the whole message");
}

/// The child of [`an_oversize_datagram_crosses_a_1500_mtu_link`].
#[tokio::test]
#[ignore = "child probe — runs only inside the slow lane's namespace"]
async fn fragmentation_probe() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        use sip_net::fragmentation::{mtu_discover_mode, PMTUDISC_DO};
        use sip_net::real::build_bound_socket;

        let probe = build_bound_socket("127.0.0.1:0".parse().unwrap(), false).unwrap();
        let mode = mtu_discover_mode(probe.as_raw_fd(), true).unwrap();
        assert_ne!(
            mode, PMTUDISC_DO,
            "under IP_PMTUDISC_DO this send returns EMSGSIZE instead of fragmenting",
        );
    }

    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64)).await.unwrap();

    let big = vec![b'z'; 4_000];
    a.send_to(&big, b.local_addr()).await.expect("the 4000-byte send succeeds");

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("the fragmented datagram must arrive")
        .expect("queue closed");
    assert_eq!(pkt.raw.len(), 4_000, "reassembled whole");
    println!("{PROBE_RAN}");
}

/// The child of [`a_dropped_non_first_fragment_loses_the_whole_message`].
#[tokio::test]
#[ignore = "child probe — runs only inside the slow lane's namespace"]
async fn fragment_loss_probe() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64)).await.unwrap();

    let big = vec![b'z'; 4_000];
    a.send_to(&big, b.local_addr())
        .await
        .expect("the send still succeeds — the loss is downstream");

    let nothing = timeout(Duration::from_millis(500), b.recv()).await;
    assert!(nothing.is_err(), "a message missing a fragment is never delivered in part");

    // A datagram that needs no fragment still crosses, so the drop rule is
    // selective — the receiver is not simply deaf.
    let small = vec![b'z'; 1_000];
    a.send_to(&small, b.local_addr()).await.unwrap();
    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("an unfragmented datagram still arrives")
        .expect("queue closed");
    assert_eq!(pkt.raw.len(), 1_000);
    println!("{PROBE_RAN}");
}
