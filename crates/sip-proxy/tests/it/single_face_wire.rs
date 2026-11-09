//! A single-face proxy's wire, byte for byte: one callflow of every kind of
//! forward, relay and own final, each datagram compared whole with
//! `single_face_wire.golden`. Dual-face plumbing changes nothing here, the
//! socket and the advertise being the one face's. `SINGLE_FACE_WIRE_UPDATE=1`
//! rewrites the file: a diff of it is a change on the wire of every single-face
//! deployment.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, RoutingStrategy};

const PROXY: &str = "192.0.2.250:5060";
const WORKER: &str = "10.0.0.8:5060";
const CALLER: &str = "198.51.100.10:5060";
const CALLEE: &str = "198.51.100.20:5060";
const HOP: &str = "198.51.100.30:5060";

fn addr(at: &str) -> SocketAddr {
    at.parse().expect("an address")
}

struct Wire {
    worker: Box<dyn UdpEndpoint>,
    caller: Box<dyn UdpEndpoint>,
    callee: Box<dyn UdpEndpoint>,
    hop: Box<dyn UdpEndpoint>,
    seen: String,
    step: u32,
}

impl Wire {
    /// `from` sends `datagram` to the proxy; whatever then reaches a party goes
    /// on the record under `what`.
    async fn step(&mut self, what: &str, from: &str, datagram: String) {
        let sender = match from {
            WORKER => &self.worker,
            CALLER => &self.caller,
            CALLEE => &self.callee,
            _ => &self.hop,
        };
        sender.send_to(datagram.as_bytes(), addr(PROXY)).await.expect("sent");
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.step += 1;
        self.seen.push_str(&format!("=== {} {what}\n", self.step));
        for (name, party) in [
            ("worker", &self.worker),
            ("caller", &self.caller),
            ("callee", &self.callee),
            ("hop", &self.hop),
        ] {
            while let Some(packet) = party.try_recv() {
                let text = String::from_utf8_lossy(&packet.raw).replace("\r\n", "\n");
                self.seen.push_str(&format!("--- {name} from {}\n{text}", packet.src));
            }
        }
    }
}

fn request(line: &str, via: &str, route: &str, to_tag: &str, cseq: &str, hops: &str) -> String {
    format!(
        "{line} SIP/2.0\r\n\
Via: SIP/2.0/UDP {via};branch=z9hG4bKwire;rport\r\n\
Max-Forwards: {hops}\r\n\
{route}From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>{to_tag}\r\n\
Call-ID: wire@example.com\r\n\
CSeq: {cseq}\r\n\
Contact: <sip:party@{via}>\r\n\
Content-Length: 0\r\n\r\n"
    )
}

/// A response whose hops are `vias`, the first the proxy's.
/// A 2xx to an INVITE names its remote target (RFC 3261 §12.1.1).
fn response(status: &str, vias: [&str; 2], cseq: &str) -> String {
    let contact = match status.starts_with('2') && cseq.ends_with("INVITE") {
        true => "Contact: <sip:bob@198.51.100.20:5060>\r\n",
        false => "",
    };
    format!(
        "SIP/2.0 {status}\r\n\
Via: SIP/2.0/UDP {};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {};branch=z9hG4bKwire;rport=5060;received={}\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: wire@example.com\r\n\
CSeq: {cseq}\r\n\
{contact}Content-Length: 0\r\n\r\n",
        vias[0],
        vias[1],
        vias[1].split(':').next().expect("a host"),
    )
}

#[tokio::test(start_paused = true)]
async fn a_single_face_proxys_wire_is_this() {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 64));
    let endpoint = bind(PROXY).await.expect("bind the proxy");
    let mut wire = Wire {
        worker: bind(WORKER).await.expect("bind the worker"),
        caller: bind(CALLER).await.expect("bind the caller"),
        callee: bind(CALLEE).await.expect("bind the callee"),
        hop: bind(HOP).await.expect("bind the hop"),
        seen: String::new(),
        step: 0,
    };
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
    let registry: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::from(addr(WORKER)),
        )]));
    let id_gen = Arc::new(sip_txn::IdGen::seeded(0x51F));
    let core = ProxyCoreBuilder::new(
        ProxyAddr::from(addr(PROXY)),
        strategy,
        registry,
        id_gen,
        sip_message::CustomParser::new(),
    )
    .clock(Clock::test_at(0))
    .build(endpoint);
    let task = tokio::spawn(core.run());

    let cookie = format!("Route: <sip:{PROXY};target={WORKER};lr>\r\n");
    let marker = format!("Route: <sip:{PROXY};outbound;lr>\r\n");
    let preload = format!("Route: <sip:{PROXY};lr>\r\n");
    let loose = format!("Route: <sip:{PROXY};outbound;lr>, <sip:{HOP};lr>\r\n");
    let (caller_uri, callee_uri) = ("sip:alice@198.51.100.10:5060", "sip:bob@198.51.100.20:5060");
    let tagged = ";tag=b";

    let invite = request(&format!("INVITE {callee_uri}"), CALLER, "", "", "1 INVITE", "70");
    wire.step("a caller's INVITE", CALLER, invite).await;
    let ringing = response("180 Ringing", [PROXY, CALLER], "1 INVITE");
    wire.step("the worker's 180", WORKER, ringing).await;
    let dial = request(&format!("INVITE {callee_uri}"), WORKER, &preload, "", "1 INVITE", "70");
    wire.step("the worker's INVITE", WORKER, dial).await;
    let cancel = request(&format!("CANCEL {callee_uri}"), WORKER, &preload, "", "1 CANCEL", "70");
    wire.step("its CANCEL", WORKER, cancel).await;
    let answer = response("200 OK", [PROXY, WORKER], "1 INVITE");
    wire.step("the callee's 200", CALLEE, answer).await;
    let by_cookie =
        request("BYE sip:b2bua@192.0.2.250:5060", CALLER, &cookie, tagged, "2 BYE", "70");
    wire.step("a caller's BYE on the cookie", CALLER, by_cookie).await;
    let keepalive =
        request(&format!("OPTIONS {caller_uri}"), WORKER, &marker, tagged, "3 OPTIONS", "70");
    wire.step("the worker's keepalive", WORKER, keepalive).await;
    let reinvite =
        request(&format!("INVITE {callee_uri}"), WORKER, &loose, tagged, "4 INVITE", "70");
    wire.step("the worker's request past a loose hop", WORKER, reinvite).await;
    let spent = request(&format!("OPTIONS {callee_uri}"), CALLER, "", "", "5 OPTIONS", "0");
    wire.step("a request out of hops: 483", CALLER, spent).await;
    let unread = request(&format!("INVITE {callee_uri}"), CALLER, "", "", "6 INVITE", "abc");
    wire.step("a request the parser refuses: 400", CALLER, unread).await;
    task.abort();

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/single_face_wire.golden");
    if std::env::var_os("SINGLE_FACE_WIRE_UPDATE").is_some() {
        std::fs::write(&golden, &wire.seen).expect("write the golden");
    }
    let held = std::fs::read_to_string(&golden).expect("read the golden");
    assert_eq!(wire.seen, held, "a change on a single-face proxy's wire");
}
