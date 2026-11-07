//! Two proxy instances behind one advertised address, as an ECMP next hop
//! spreads them: each instance binds its own node address and advertises the
//! shared one, so every Via and Record-Route it writes names the shared
//! address, and each party's flow is delivered to the instance the test
//! assigns it. Both instances share the cookie key and the key of their own
//! finals' To-tags; each keeps its own branch memo, worker view and AIMD
//! buckets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::cancel_lru::{CancelBranchLru, MemoBounds};
use sip_proxy::load_observer::{LoadObserverConfig, OverloadPayload, WorkerLoadObserver};
use sip_proxy::refusals::Refusals;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::security::own_final_tag::own_final_tagger;
use sip_proxy::strategies::rendezvous::{rendezvous_select, RendezvousCandidate};
use sip_proxy::{LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

/// The address both instances advertise.
pub const SHARED: (&str, u16) = ("192.0.2.250", 5060);

/// The cookie key every instance of the deployment holds.
const COOKIE_KEY: [u8; 32] = [7u8; 32];

/// How long a party waits for a datagram before the cell fails.
const RECV_BOUND: Duration = Duration::from_secs(5);

/// One of the two instances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    A,
    B,
}

/// A party whose flow an ECMP hop delivers to one instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Party {
    /// The upstream peer the caller's requests arrive from.
    Caller,
    /// The downstream peer a worker's outbound requests go to.
    Callee,
    W1,
    W2,
    /// A hop a worker's preloaded route names past the proxy.
    Relay,
}

impl Party {
    pub fn addr(self) -> SocketAddr {
        let at = match self {
            Party::Caller => "198.51.100.10:5060",
            Party::Callee => "198.51.100.20:5060",
            Party::W1 => "10.0.0.2:5070",
            Party::W2 => "10.0.0.3:5070",
            Party::Relay => "198.51.100.30:5060",
        };
        at.parse().expect("a socket address")
    }

    /// The worker ordinal, for a worker.
    pub fn ordinal(self) -> &'static str {
        match self {
            Party::W1 => "w1",
            Party::W2 => "w2",
            other => panic!("{other:?} is no worker"),
        }
    }

    /// The other worker, for a worker.
    pub fn other_worker(self) -> Party {
        match self {
            Party::W1 => Party::W2,
            Party::W2 => Party::W1,
            other => panic!("{other:?} is no worker"),
        }
    }

    fn all() -> [Party; 5] {
        [Party::Caller, Party::Callee, Party::W1, Party::W2, Party::Relay]
    }
}

const WORKERS: [Party; 2] = [Party::W1, Party::W2];

struct Ordinal(&'static str);

impl RendezvousCandidate for Ordinal {
    fn id(&self) -> &str {
        self.0
    }
}

/// `count` Call-IDs, prefixed `prefix`, whose rendezvous winner is `worker`.
pub fn calls_won_by(worker: Party, prefix: &str, count: usize) -> Vec<String> {
    (0..)
        .map(|n| format!("{prefix}-{n}@caller"))
        .filter(|c| winner_of(c) == worker)
        .take(count)
        .collect()
}

/// The worker the rendezvous ranks first for `call_id` among both workers:
/// where a call lands when nothing filters or spills it.
pub fn winner_of(call_id: &str) -> Party {
    let candidates = [Ordinal("w1"), Ordinal("w2")];
    let winner = rendezvous_select(call_id, &candidates).expect("two candidates");
    if winner.0 == "w1" {
        Party::W1
    } else {
        Party::W2
    }
}

/// How an instance is built.
#[derive(Clone, Copy, Default)]
pub struct InstanceOpts {
    pub memo: MemoBounds,
}

/// One running instance and the handles a cell steers it by.
pub struct Instance {
    pub node: SocketAddr,
    pub registry: SimulatedWorkerRegistry,
    pub observer: Arc<WorkerLoadObserver>,
    pub memo: Arc<CancelBranchLru>,
    clock: Clock,
    task: JoinHandle<()>,
}

impl Instance {
    /// Spend every token of this instance's AIMD bucket for `worker`.
    pub fn empty_bucket(&self, worker: Party) {
        let now = self.clock.now_ms();
        while self.observer.try_consume_for(worker.ordinal(), now).is_ok() {}
    }

    /// This instance reads `worker` in the above-critical band.
    pub fn read_critical(&self, worker: Party) {
        let hot = OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 };
        self.observer.apply_payload(worker.ordinal(), &hot, self.clock.now_ms());
    }

    /// This instance reads `worker` taking new calls or refusing them.
    pub fn accepting(&self, worker: Party, accepts: bool) {
        let acceptance = if accepts { Acceptance::Accept } else { Acceptance::Refuse };
        self.registry.set_acceptance(worker.ordinal(), acceptance);
    }

    /// This instance reads `worker` with `health`.
    pub fn read(&self, worker: Party, health: WorkerHealth) {
        self.registry.set_health(worker.ordinal(), health);
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The two instances, the parties, and which instance each party's flow
/// reaches.
pub struct Pair {
    pub a: Instance,
    pub b: Instance,
    parties: HashMap<Party, Box<dyn UdpEndpoint>>,
    flows: Mutex<HashMap<Party, Side>>,
}

impl Pair {
    /// Both instances with default options, every flow on A.
    pub async fn new() -> Pair {
        Self::with(InstanceOpts::default(), InstanceOpts::default()).await
    }

    /// Instances A and B built from `a` and `b`, every flow on A.
    pub async fn with(a: InstanceOpts, b: InstanceOpts) -> Pair {
        let net = SimulatedSignalingNetwork::new(1);
        let clock = Clock::test_at(0);
        let a = instance(&net, &clock, "10.0.0.101:5060", 0xA, a).await;
        let b = instance(&net, &clock, "10.0.0.102:5060", 0xB, b).await;
        let mut parties = HashMap::new();
        for party in Party::all() {
            let ep = net.bind_udp(BindUdpOpts::new(party.addr(), 64)).await.expect("party binds");
            parties.insert(party, ep);
        }
        let flows = Party::all().into_iter().map(|p| (p, Side::A)).collect();
        Pair { a, b, parties, flows: Mutex::new(flows) }
    }

    pub fn instance(&self, side: Side) -> &Instance {
        match side {
            Side::A => &self.a,
            Side::B => &self.b,
        }
    }

    /// From now on `party`'s flow reaches `side`.
    pub fn assign(&self, party: Party, side: Side) {
        self.flows.lock().unwrap().insert(party, side);
    }

    fn endpoint(&self, party: Party) -> &dyn UdpEndpoint {
        self.parties.get(&party).expect("every party is bound").as_ref()
    }

    /// `from` sends `msg` to the shared address: its flow's instance gets it.
    pub async fn send(&self, from: Party, msg: &SipMessage) {
        let side = *self.flows.lock().unwrap().get(&from).expect("every party has a flow");
        let node = self.instance(side).node;
        let bytes = match msg {
            SipMessage::Request(r) => r.image().to_vec(),
            SipMessage::Response(r) => r.image().to_vec(),
        };
        self.endpoint(from).send_to(&bytes, node).await.expect("the fabric takes the datagram");
    }

    /// The next datagram `at` receives, with the instance node it came from.
    pub async fn recv(&self, at: Party) -> (SipMessage, SocketAddr) {
        let pkt = tokio::time::timeout(RECV_BOUND, self.endpoint(at).recv())
            .await
            .unwrap_or_else(|_| panic!("{at:?} receives nothing"))
            .unwrap_or_else(|| panic!("{at:?}'s endpoint closed"));
        let msg = CustomParser::new().parse(&pkt.raw).expect("the proxy sends SIP");
        (msg, pkt.src)
    }

    /// The next request `at` receives, of `method`.
    pub async fn request(&self, at: Party, method: &str) -> (SipRequest, SocketAddr) {
        match self.recv(at).await {
            (SipMessage::Request(r), src) if r.method().as_str() == method => (r, src),
            (other, _) => panic!("{at:?} expected a {method}, got {:?}", start_line(&other)),
        }
    }

    /// The next response `at` receives, of `status`.
    pub async fn response(&self, at: Party, status: u16) -> SipResponse {
        match self.recv(at).await {
            (SipMessage::Response(r), _) if r.status() == status => r,
            (other, _) => panic!("{at:?} expected a {status}, got {:?}", start_line(&other)),
        }
    }

    /// `at` receives nothing within one second.
    pub async fn silent(&self, at: Party) {
        let got = tokio::time::timeout(Duration::from_secs(1), self.endpoint(at).recv()).await;
        if let Ok(Some(pkt)) = got {
            let line = String::from_utf8_lossy(&pkt.raw).lines().next().unwrap_or("").to_string();
            panic!("{at:?} receives {line:?}, expected nothing");
        }
    }
}

fn start_line(msg: &SipMessage) -> String {
    let image = match msg {
        SipMessage::Request(r) => r.image(),
        SipMessage::Response(r) => r.image(),
    };
    String::from_utf8_lossy(image).lines().next().unwrap_or("").to_string()
}

async fn instance(
    net: &SimulatedSignalingNetwork,
    clock: &Clock,
    node: &str,
    seed: u64,
    opts: InstanceOpts,
) -> Instance {
    let node: SocketAddr = node.parse().expect("a node address");
    let ep = net.bind_udp(BindUdpOpts::new(node, 256)).await.expect("the instance binds");
    let entries = WORKERS
        .iter()
        .map(|w| {
            let at = w.addr();
            WorkerEntry::alive(w.ordinal(), ProxyAddr::new(at.ip().to_string(), at.port()))
        })
        .collect();
    let registry = SimulatedWorkerRegistry::with_clock(entries, clock.clone());
    let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let cool = OverloadPayload { elu: 0.1, gc: 0.0, adm: 0.0 };
    for w in WORKERS {
        observer.apply_payload(w.ordinal(), &cool, clock.now_ms());
    }
    let hmac = Arc::new(
        StaticHmacKeyProvider::new(HmacKey::new("k1", COOKIE_KEY.to_vec()), None)
            .expect("a valid key"),
    );
    let metrics = Arc::new(ProxyMetrics::new());
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry_dyn.clone(),
        hmac,
        observer.clone(),
        metrics.clone(),
        clock.clone(),
    ));
    let id_gen = Arc::new(IdGen::seeded(seed));
    let tagger = own_final_tagger(&COOKIE_KEY, &id_gen);
    let refusals = Arc::new(Refusals::keyed(tagger, 0, sip_proxy::refusals::REFUSED_MEMO_MAX));
    let memo = Arc::new(CancelBranchLru::with_bounds(clock.clone(), opts.memo));
    let advertised = ProxyAddr::new(SHARED.0, SHARED.1);
    let core = ProxyCoreBuilder::new(
        advertised,
        strategy,
        registry_dyn,
        id_gen,
        sip_message::CustomParser::new(),
    )
    .clock(clock.clone())
    .metrics(metrics)
    .refusals(refusals)
    .cancel_lru(memo.clone())
    .build(ep);
    let task = tokio::spawn(core.run());
    Instance { node, registry, observer, memo, clock: clock.clone(), task }
}
