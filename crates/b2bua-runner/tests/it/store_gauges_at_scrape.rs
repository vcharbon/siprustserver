//! The runner binary's `/metrics` reads its call store as it is at the
//! scrape: an observer that waits for `b2bua_store_calls` and
//! `b2bua_store_sip_index` to read 0 after a call (a batch's cleanup gate) sees
//! the call while it is held and its removal at once, never a sample taken
//! before the call began. alice calls bob through the binary on real loopback
//! UDP; bob is the binary's default callee.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use layer_harness::TransportKind;
use scenario_harness::{Harness, ANSWER_SDP, OFFER_SDP};
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};

/// The store gauges the scrape reads.
const STORE: [&str; 2] = ["b2bua_store_calls", "b2bua_store_sip_index"];

/// A loopback UDP address no socket held a moment ago.
fn free_udp() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).expect("a free UDP port")
}

/// A loopback TCP address no listener held a moment ago.
fn free_tcp() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("a free TCP port")
}

/// The running binary, killed on drop.
struct Runner {
    child: Child,
    sip: SocketAddr,
    metrics: SocketAddr,
    cdr: PathBuf,
}

impl Runner {
    /// The binary standalone on loopback, every call routed to `callee`, its
    /// call records appended to a file of its own; returned once `/metrics`
    /// answers.
    fn start(callee: SocketAddr) -> Runner {
        let (sip, metrics, admin) = (free_udp(), free_tcp(), free_tcp());
        let cdr = std::env::temp_dir()
            .join(format!("b2bua-runner-store-gauges-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&cdr);
        let child = Command::new(env!("CARGO_BIN_EXE_b2bua-runner"))
            .env("B2BUA_MODE", "standalone")
            .env("B2BUA_LISTEN", sip.to_string())
            .env("B2BUA_ADVERTISE", sip.to_string())
            .env("B2BUA_DEST", callee.to_string())
            .env("B2BUA_METRICS", metrics.to_string())
            .env("B2BUA_ADMIN", admin.to_string())
            .env("B2BUA_CDR_FILE", &cdr)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the runner starts");
        let runner = Runner { child, sip, metrics, cdr };
        let deadline = Instant::now() + Duration::from_secs(20);
        while runner.scrape().is_none() {
            assert!(Instant::now() < deadline, "the runner's /metrics never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
        runner
    }

    /// The `/metrics` body, `None` while nothing answers.
    fn scrape(&self) -> Option<String> {
        let mut s = TcpStream::connect(self.metrics).ok()?;
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").ok()?;
        let mut resp = String::new();
        s.read_to_string(&mut resp).ok()?;
        resp.starts_with("HTTP/1.1 200").then_some(resp)
    }

    /// Each of [`STORE`] as one scrape reads it.
    fn store(&self) -> [u64; 2] {
        let body = self.scrape().expect("a /metrics body");
        STORE.map(|name| {
            body.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
                .unwrap_or_else(|| panic!("{name} in:\n{body}"))
        })
    }

    /// [`store`](Self::store) once both read 0, or the last read after 2 s.
    fn store_emptied(&self) -> [u64; 2] {
        settled(|| self.store(), |read| read == &[0, 0])
    }

    /// The call records the binary appended, one per line, once there is
    /// one, or the last count after 2 s.
    fn records(&self) -> usize {
        let count =
            || std::fs::read_to_string(&self.cdr).map(|text| text.lines().count()).unwrap_or(0);
        settled(count, |n| *n > 0)
    }
}

/// `read()` once `done` holds for it, or the last read after 2 s.
fn settled<T>(read: impl Fn() -> T, done: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let value = read();
        if done(&value) || Instant::now() >= deadline {
            return value;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cdr);
    }
}

/// An answered call reads in the store gauges while it is held, within the
/// gauge sampler's period of the binary's start, and they read 0 once it is
/// hung up.
#[tokio::test]
async fn a_held_call_reads_in_the_store_gauges_at_once_and_its_hangup_empties_them() {
    let h = Harness::with_network_and_clock(
        "runner-store-gauges",
        Arc::new(RealSignalingNetwork::new()) as Arc<dyn SignalingNetwork>,
        Clock::system(),
        TransportKind::Live,
        Duration::from_secs(5),
    );
    let alice = h.agent("alice", &free_udp().to_string()).await;
    let bob = h.agent("bob", &free_udp().to_string()).await;
    let runner = Runner::start(bob.addr());

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(runner.sip).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let [calls, indexed] = runner.store();
    assert_eq!(calls, 1, "the held call is in the call map");
    assert!(indexed >= 1, "the held call's dialogs are in the SIP index");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(runner.store_emptied(), [0, 0], "the hung-up call left the store");
    assert_eq!(runner.records(), 1, "one call record");
    let _report = h.finish().await;
}
