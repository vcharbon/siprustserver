//! The bind retry, over real sockets.
use super::*;

// ── bind_with_retry — bounded EADDRINUSE wait ───────────────────────────
//
// Real sockets ⇒ real clock (docs/testing/test-clock.md forbids pausing
// tokio time under real socket readiness). Intervals are test-short
// (10 ms / 300 ms) so the whole group stays sub-second — default lane.

const TEST_INTERVAL: Duration = Duration::from_millis(10);
const TEST_DEADLINE: Duration = Duration::from_millis(300);

fn tcp_in_use(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AddrInUse
}

/// (a) The holder releases the port mid-retry: the bind lands well before
/// the deadline instead of the process dying on the first attempt.
#[tokio::test]
async fn bind_retry_succeeds_once_the_holder_releases() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = holder.local_addr().unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(holder);
    });
    let bound = bind_with_retry("test", addr, TEST_INTERVAL, TEST_DEADLINE, tcp_in_use, || {
        tokio::net::TcpListener::bind(addr)
    })
    .await
    .expect("retry must win once the holder releases");
    assert_eq!(bound.local_addr().unwrap(), addr);
    release.await.unwrap();
}

/// (b) The holder never releases: the deadline expires and the error names
/// the contested address. Exercises the UDP side end-to-end — the real
/// network's `BindError` classifies EADDRINUSE structurally.
#[tokio::test]
async fn bind_retry_gives_up_at_the_deadline_naming_the_addr() {
    let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = holder.local_addr().unwrap();
    let net = RealSignalingNetwork::new();
    let err = bind_with_retry(
        "test",
        addr,
        TEST_INTERVAL,
        TEST_DEADLINE,
        |e: &BindError| e.is_addr_in_use(),
        || async move { net.bind_udp(BindUdpOpts::new(addr, 8)).await },
    )
    .await;
    // Manual unwrap of the Err arm — `Box<dyn UdpEndpoint>` is not Debug.
    let err = match err {
        Ok(_) => panic!("the addr stays held — the deadline must expire"),
        Err(e) => e,
    };
    assert!(err.contains(&addr.to_string()), "error must name the addr: {err}");
    assert!(err.contains("still in use"), "{err}");
    drop(holder);
}

/// A non-addr-in-use failure is NOT retried — it surfaces immediately
/// (still naming the address) so a genuine misconfiguration fails fast.
#[tokio::test]
async fn bind_retry_fails_fast_on_a_non_in_use_error() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = holder.local_addr().unwrap();
    let started = std::time::Instant::now();
    let err = bind_with_retry(
        "test",
        addr,
        TEST_INTERVAL,
        TEST_DEADLINE,
        |_: &std::io::Error| false,
        || tokio::net::TcpListener::bind(addr),
    )
    .await
    .expect_err("a non-retryable error must surface at once");
    assert!(started.elapsed() < TEST_DEADLINE, "must not wait out the deadline");
    assert!(err.contains(&addr.to_string()) && err.contains("failed"), "{err}");
}

/// The limit the configuration states is the one the signalling socket's intake
/// hook reads under: a well-formed OPTIONS past it is queued with its refusal.
/// The default limits read the same datagram.
#[tokio::test]
async fn the_intake_hook_reads_under_the_limit_the_configuration_states() {
    let lookup = |key: &str| match key {
        "PROXY_LISTEN" => Some("127.0.0.1:0".to_string()),
        "PROXY_METRICS" => Some("127.0.0.1:0".to_string()),
        "PROXY_WORKERS" => Some("w0@10.0.0.1:5060".to_string()),
        "PROXY_HMAC_KEY" => Some("0123456789abcdef0123456789abcdef".to_string()),
        _ => None,
    };
    let options = b"OPTIONS sip:bob@192.0.2.30 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-limit\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@192.0.2.20>;tag=alice-tag\r\n\
        To: <sip:bob@192.0.2.30>\r\n\
        Call-ID: limit@192.0.2.20\r\n\
        CSeq: 1 OPTIONS\r\n\
        Content-Length: 0\r\n\r\n";
    let read_under = |max_header_lines: usize| {
        let mut cfg = ProxyConfig::from_lookup(&lookup, &|| None).expect("a valid boot");
        cfg.parser_limits.max_header_lines = max_header_lines;
        async move {
            let metrics = Arc::new(ProxyMetrics::new());
            let sockets = bind_sockets(&cfg, &Clock::system(), &metrics).await.expect("bound");
            let shard = sockets.shards.into_iter().next().expect("one shard");
            let to = shard.internal.local_addr();
            let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("the peer");
            peer.send_to(options, to).await.expect("sent");
            let packet = tokio::time::timeout(Duration::from_millis(500), shard.internal.recv())
                .await
                .expect("in time")
                .expect("the endpoint is open");
            packet.parsed.expect("the hook parsed it")
        }
    };
    let refused = read_under(3).await;
    assert!(matches!(&*refused, Ok(Err(r)) if r.reason.contains("exceeds limit 3")), "{refused:?}");
    let read = read_under(100).await;
    assert!(matches!(&*read, Ok(Ok(_))), "{read:?}");
}
