//! Multiplexed SIP transport — a SIPp-style replacement for one-socket-per-call.
//!
//! # Shape
//!
//! A [`MuxCore`] owns a small, fixed set of **named endpoints**, each = exactly
//! one real UDP socket with a dispatcher recv-loop (no sharded pool). Each socket
//! multiplexes *many* concurrent dialogs, routing inbound datagrams to the right
//! call's inbox by dialog identity. So O(endpoints) sockets regardless of CPS —
//! no fd / ephemeral-port exhaustion, and the SUT routes the callee leg to a
//! **static** address (its own config), never a dynamic per-call one.
//!
//! # Correlation (SUT-agnostic, header-only)
//!
//! Each call carries one random token in a single transparent header
//! (`X-Loadgen-Id`). Demux precedence per inbound datagram:
//! 1. **known Call-ID** — our UAC dialog (Call-ID sniffed from our outbound
//!    INVITE), or a UAS dialog *after* its first request (the SUT-minted Call-ID
//!    we learned from the INVITE).
//! 2. **correlation token** — an inbound *initial* request whose Call-ID we've
//!    never seen, matched against the pending-UAS registry by the
//!    [`Correlation`] header value. A proxy / B2BUA that relays the header
//!    forwards the token unchanged onto **every** originated leg, so a call's
//!    callee (bob) and transfer-target (charlie) legs share one token. No
//!    To-/Request-URI hijacking — that breaks against any SUT that routes on
//!    those URIs.
//!
//! # Recording
//!
//! The mux is a [`SignalingNetwork`]: a bare [`MuxEndpoint`] is the real-UDP +
//! demux layer with no recording cost. The existing
//! `AgentBinder::with_network(mux, …, record)` wraps it with the recording fake
//! layer **only for sampled calls** — so recording sits *after* the UDP/demux
//! layer, per call, free when off.
//!
//! # No leak / observability
//!
//! Every per-call endpoint deregisters its keys on `Drop`; a reaper sweeps
//! pending-UAS entries whose callee leg never arrived; inboxes are bounded. A
//! callee dialog with **no** token, or a token matching **no** pending call, is
//! counted (`mux_orphan_total{reason}`) + bounded-sampled + dropped — never
//! queued, never silent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use sip_net::queue::PacketQueue;
use sip_net::{
    BindError, BindErrorReason, BindUdpOpts, SendError, SignalingNetwork, UdpEndpoint,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// Whether a defined endpoint originates calls (UAC) or receives them (UAS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Caller: registers its dialog by the Call-ID sniffed from its outbound
    /// INVITE; receives responses + in-dialog by that Call-ID.
    Caller,
    /// Callee: registers a correlation token at bind; the SUT-delivered initial
    /// INVITE is matched by token, then promoted to its Call-ID.
    Callee,
}

/// How the per-call correlation token is carried through the SUT: a single
/// transparent header (e.g. `X-Loadgen-Id`). SUT-agnostic — a proxy/B2BUA that
/// relays the header forwards the token unchanged onto every originated leg, so
/// one call's bob and charlie legs share one token. No To-/R-URI hijacking.
#[derive(Debug, Clone)]
pub struct Correlation {
    header: String,
}

impl Correlation {
    /// A single transparent correlation header (proxies/B2BUAs forward it).
    pub fn header(name: impl Into<String>) -> Self {
        Self { header: name.into() }
    }
    /// The header name a scenario stamps the per-call token into.
    pub fn header_name(&self) -> &str {
        &self.header
    }
    /// The correlation token carried by `raw`, if the header is present.
    fn token(&self, raw: &[u8]) -> Option<String> {
        header_value(raw, &self.header)
    }
}

/// A read-only view of an inbound initial INVITE handed to a scenario-owned
/// [`LegPicker`]. The mux uses it for **nothing** itself (it only correlates the
/// call by token); it exists purely so a scenario can disambiguate which of its
/// receivers should own a new leg, keying on whatever it likes (R-URI, To,
/// `X-Api-Call`, a custom header). The scenario — not the mux — owns the meaning
/// of these fields.
pub struct LegInfo<'a> {
    raw: &'a [u8],
}

impl LegInfo<'_> {
    /// The raw datagram bytes.
    pub fn raw(&self) -> &[u8] {
        self.raw
    }
    /// Value of header `name` (case-insensitive), or `None`.
    pub fn header(&self, name: &str) -> Option<String> {
        header_value(self.raw, name)
    }
    /// The Request-URI (the full 2nd token of the request line).
    pub fn ruri(&self) -> Option<String> {
        ruri(self.raw)
    }
    /// The Request-URI user-part (e.g. `dave` from `sip:dave@host`).
    pub fn ruri_user(&self) -> Option<String> {
        self.ruri().as_deref().and_then(uri_user)
    }
    /// The To header user-part.
    pub fn to_user(&self) -> Option<String> {
        self.header("to").or_else(|| self.header("t")).as_deref().and_then(uri_user)
    }
}

/// A scenario-owned callback that picks which receiver (by its label = the bound
/// agent's name) should own a freshly-arrived leg, when **more than one**
/// receiver shares a single socket for the same call. Invoked only on that
/// ambiguity; a single-receiver socket never calls it. Returning a label that
/// matches no receiver drops the leg as a `no_route` orphan.
///
/// Contract: the picker is called **while the mux holds the socket registry
/// lock**, so it MUST be pure-ish — it must not re-enter the mux (e.g. call
/// `registry_size`/bind/drop on the same core: self-deadlock). A panic is
/// contained (the leg becomes a `no_route` orphan), not propagated.
pub type LegPicker = Arc<dyn Fn(&LegInfo) -> String + Send + Sync>;

/// A registry key owned by one endpoint (removed on its `Drop`).
#[derive(Debug, Clone)]
enum Key {
    CallId(String),
    /// A receiver registered under `token`, identified by its `label` (agent
    /// name) so `Drop` removes exactly this receiver from a possibly-shared slot.
    Token { token: String, label: String },
}

/// Process-wide mux counters (Prometheus + report).
#[derive(Default)]
pub struct MuxStats {
    pub orphan_no_header: AtomicU64,
    pub orphan_unknown_token: AtomicU64,
    pub orphan_stray: AtomicU64,
    pub pending_expired: AtomicU64,
    pub inbox_drop: AtomicU64,
    pub delivered: AtomicU64,
    sample_cap: usize,
    samples: Mutex<Vec<String>>,
}

impl MuxStats {
    fn new(sample_cap: usize) -> Self {
        Self { sample_cap, ..Default::default() }
    }
    fn orphan(&self, reason: OrphanReason, raw: &[u8]) {
        match reason {
            OrphanReason::NoHeader => self.orphan_no_header.fetch_add(1, Ordering::Relaxed),
            OrphanReason::UnknownToken | OrphanReason::NoRoute => {
                self.orphan_unknown_token.fetch_add(1, Ordering::Relaxed)
            }
            OrphanReason::Stray => self.orphan_stray.fetch_add(1, Ordering::Relaxed),
        };
        let mut g = self.samples.lock().unwrap();
        if g.len() < self.sample_cap {
            g.push(format!("[{}] {}", reason.label(), first_line(raw)));
        }
    }
    /// Bounded orphan samples (the "notify" surface).
    pub fn samples(&self) -> Vec<String> {
        self.samples.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy)]
enum OrphanReason {
    /// An initial INVITE we cannot correlate (no token) — the concerning case.
    NoHeader,
    /// A token present but matching no pending call.
    UnknownToken,
    /// A token matched a call, but its scenario-owned picker chose a label no
    /// registered receiver carries (a scenario routing bug).
    NoRoute,
    /// An unknown Call-ID that is not an initial INVITE (a late straggler).
    Stray,
}
impl OrphanReason {
    fn label(self) -> &'static str {
        match self {
            OrphanReason::NoHeader => "no_header",
            OrphanReason::UnknownToken => "unknown_token",
            OrphanReason::NoRoute => "no_route",
            OrphanReason::Stray => "stray",
        }
    }
}

/// One receiver bound on a socket: an inbox + its label (the agent name a
/// [`LegPicker`] returns) + the keyset its `Drop` drains.
struct ReceiverEntry {
    label: String,
    queue: Arc<PacketQueue>,
    keyset: Arc<Mutex<Vec<Key>>>,
}

/// All receivers for one call on one socket, sharing the call token. Usually a
/// single receiver; >1 only when a scenario binds several endpoints on the same
/// port, in which case `picker` disambiguates each arriving leg.
struct CallSlot {
    receivers: Vec<ReceiverEntry>,
    /// Scenario-owned disambiguation, used iff `receivers.len() > 1`.
    picker: Option<LegPicker>,
    /// Set once any leg has arrived — the reaper then leaves the slot alone
    /// (a live call may outlast the pending deadline), so only never-arrived
    /// legs are swept.
    arrived: bool,
    deadline: Instant,
}

#[derive(Default)]
struct SocketRegistry {
    by_call_id: HashMap<String, Arc<PacketQueue>>,
    by_token: HashMap<String, CallSlot>,
}

/// One defined endpoint = one real socket + dispatcher + per-socket registry.
struct MuxSocket {
    addr: SocketAddr,
    role: Role,
    socket: Arc<UdpSocket>,
    reg: Mutex<SocketRegistry>,
    correlation: Correlation,
    queue_max: usize,
    stats: Arc<MuxStats>,
    _dispatcher: JoinHandle<()>,
}

/// The process-wide multiplexer: the fixed set of endpoints + shared stats +
/// the pending-entry reaper.
pub struct MuxCore {
    endpoints: HashMap<SocketAddr, Arc<MuxSocket>>,
    stats: Arc<MuxStats>,
    pending_ttl: Duration,
    _reaper: JoinHandle<()>,
}

/// One endpoint to open.
pub struct EndpointSpec {
    pub addr: SocketAddr,
    pub role: Role,
}

impl MuxCore {
    /// Open every endpoint's socket + dispatcher and start the reaper. `queue_max`
    /// bounds each call's inbox; `orphan_sample_cap` bounds stored orphan samples.
    pub async fn bind(
        specs: Vec<EndpointSpec>,
        correlation: Correlation,
        queue_max: usize,
        orphan_sample_cap: usize,
        pending_ttl: Duration,
    ) -> std::io::Result<Arc<Self>> {
        let stats = Arc::new(MuxStats::new(orphan_sample_cap));
        let mut endpoints = HashMap::new();
        for spec in specs {
            let socket = Arc::new(UdpSocket::bind(spec.addr).await?);
            let local = socket.local_addr()?;
            let mux = Arc::new_cyclic(|weak: &std::sync::Weak<MuxSocket>| {
                let dispatcher = tokio::spawn(dispatch_loop(weak.clone(), socket.clone()));
                MuxSocket {
                    addr: local,
                    role: spec.role,
                    socket: socket.clone(),
                    reg: Mutex::new(SocketRegistry::default()),
                    correlation: correlation.clone(),
                    queue_max,
                    stats: stats.clone(),
                    _dispatcher: dispatcher,
                }
            });
            endpoints.insert(local, mux);
        }
        let reaper = tokio::spawn(reap_loop(
            endpoints.values().cloned().collect::<Vec<_>>(),
            stats.clone(),
            pending_ttl,
        ));
        Ok(Arc::new(Self { endpoints, stats, pending_ttl, _reaper: reaper }))
    }

    /// The bound address for a defined endpoint, by bind order index helper —
    /// callers usually keep the addresses they passed in. Returns all endpoint
    /// addresses (handy for tests).
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.endpoints.keys().copied().collect()
    }

    pub fn stats(&self) -> &Arc<MuxStats> {
        &self.stats
    }

    /// Live registry size across all endpoints (leak canary — should track
    /// in-flight calls). Counts every live receiver across all call slots plus
    /// the promoted Call-IDs.
    pub fn registry_size(&self) -> usize {
        self.endpoints
            .values()
            .map(|e| {
                let g = e.reg.lock().unwrap();
                let receivers: usize = g.by_token.values().map(|s| s.receivers.len()).sum();
                g.by_call_id.len() + receivers
            })
            .sum()
    }

    /// Build a per-call [`SignalingNetwork`] view from a [`CallRouting`] (the
    /// single call token + each callee leg's label + any same-socket picker).
    pub fn network(self: &Arc<Self>, routing: CallRouting) -> MuxNetwork {
        // Per-callee-addr bind-order label queue: each `bind_udp(addr)` dispenses
        // the next declared label for that addr (so several receivers can share a
        // socket; the driver binds them in declaration order).
        let mut labels: HashMap<SocketAddr, Vec<String>> = HashMap::new();
        for (addr, label) in &routing.legs {
            labels.entry(*addr).or_default().push(label.clone());
        }
        MuxNetwork {
            core: self.clone(),
            token: routing.token,
            labels,
            pickers: routing.pickers,
            cursor: Mutex::new(HashMap::new()),
        }
    }

    /// Render the mux Prometheus series.
    pub fn render_prometheus(&self) -> String {
        let s = &self.stats;
        let mut out = String::new();
        out.push_str("# HELP loadgen_mux_orphan_total Inbound datagrams that matched no call.\n");
        out.push_str("# TYPE loadgen_mux_orphan_total counter\n");
        out.push_str(&format!(
            "loadgen_mux_orphan_total{{reason=\"no_header\"}} {}\n",
            s.orphan_no_header.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "loadgen_mux_orphan_total{{reason=\"unknown_token\"}} {}\n",
            s.orphan_unknown_token.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "loadgen_mux_orphan_total{{reason=\"stray\"}} {}\n",
            s.orphan_stray.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP loadgen_mux_registry_size Live demux entries (leak canary).\n");
        out.push_str("# TYPE loadgen_mux_registry_size gauge\n");
        out.push_str(&format!("loadgen_mux_registry_size {}\n", self.registry_size()));
        out.push_str("# HELP loadgen_mux_pending_expired_total Pending callee legs reaped (never arrived).\n");
        out.push_str("# TYPE loadgen_mux_pending_expired_total counter\n");
        out.push_str(&format!(
            "loadgen_mux_pending_expired_total {}\n",
            s.pending_expired.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP loadgen_mux_inbox_drop_total Datagrams dropped on a full call inbox.\n");
        out.push_str("# TYPE loadgen_mux_inbox_drop_total counter\n");
        out.push_str(&format!("loadgen_mux_inbox_drop_total {}\n", s.inbox_drop.load(Ordering::Relaxed)));
        out.push_str("# HELP loadgen_mux_delivered_total Datagrams demuxed to a call.\n");
        out.push_str("# TYPE loadgen_mux_delivered_total counter\n");
        out.push_str(&format!("loadgen_mux_delivered_total {}\n", s.delivered.load(Ordering::Relaxed)));
        out
    }
}

/// The per-call routing a scenario declares before its agents bind: the single
/// correlation token every leg of the call carries, the callee legs in **bind
/// order** (`(addr, label)`, label = the agent name a picker returns), and any
/// per-socket [`LegPicker`] used to disambiguate several receivers sharing one
/// socket. The mux never reads `X-Api-Call` or any URI to route legs — that is
/// the scenario's job, expressed here (the picker) and in how it dials the SUT.
#[derive(Clone, Default)]
pub struct CallRouting {
    token: String,
    legs: Vec<(SocketAddr, String)>,
    pickers: HashMap<SocketAddr, LegPicker>,
}

impl CallRouting {
    /// Start a routing for a call with correlation token `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), legs: Vec::new(), pickers: HashMap::new() }
    }
    /// Declare a callee leg: an agent labelled `label` binds on `addr`. Declare
    /// legs in the order the driver binds the agents (several on one `addr` form
    /// a shared socket).
    pub fn leg(mut self, addr: SocketAddr, label: impl Into<String>) -> Self {
        self.legs.push((addr, label.into()));
        self
    }
    /// Attach the scenario-owned picker for a socket that carries >1 receiver.
    pub fn picker(mut self, addr: SocketAddr, picker: LegPicker) -> Self {
        self.pickers.insert(addr, picker);
        self
    }
}

/// Per-call `SignalingNetwork` over the shared [`MuxCore`].
pub struct MuxNetwork {
    core: Arc<MuxCore>,
    token: String,
    /// Bind-order labels per callee addr (dispensed by `cursor`).
    labels: HashMap<SocketAddr, Vec<String>>,
    pickers: HashMap<SocketAddr, LegPicker>,
    cursor: Mutex<HashMap<SocketAddr, usize>>,
}

#[async_trait]
impl SignalingNetwork for MuxNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        let mux = self.core.endpoints.get(&opts.addr).ok_or_else(|| BindError {
            reason: BindErrorReason::OsError,
            addr: opts.addr,
            message: format!("no mux endpoint defined at {}", opts.addr),
        })?;
        let queue = Arc::new(PacketQueue::new(mux.queue_max));
        let keyset = Arc::new(Mutex::new(Vec::new()));

        if mux.role == Role::Callee {
            // Dispense the next declared label for this addr (bind order), so
            // several receivers can share a socket.
            let label = {
                let mut cur = self.cursor.lock().unwrap();
                let n = cur.entry(opts.addr).or_insert(0);
                let idx = *n;
                *n += 1;
                self.labels.get(&opts.addr).and_then(|v| v.get(idx)).cloned().ok_or_else(|| {
                    BindError {
                        reason: BindErrorReason::OsError,
                        addr: opts.addr,
                        message: format!(
                            "callee endpoint {} bound without a declared leg (#{idx})",
                            opts.addr
                        ),
                    }
                })?
            };
            let token = self.token.clone();
            keyset.lock().unwrap().push(Key::Token { token: token.clone(), label: label.clone() });
            let mut g = mux.reg.lock().unwrap();
            let pending_ttl = self.core.pending_ttl;
            let slot = g.by_token.entry(token).or_insert_with(|| CallSlot {
                receivers: Vec::new(),
                picker: self.pickers.get(&opts.addr).cloned(),
                arrived: false,
                // The leg should arrive within the call's recv window; a generous
                // multiple guards a slow SUT while still reaping a no-show.
                // INVARIANT: this deadline (`pending_ttl * 4`) must exceed one
                // callee `recv_timeout`, or the reaper could close a LIVE (but
                // not-yet-`arrived`) receiver's queue out from under a pending
                // `try_receive`. The harness sets `pending_ttl == recv_timeout`,
                // so the 4× margin holds; keep `pending_ttl >= recv_timeout`.
                deadline: Instant::now() + pending_ttl * 4,
            });
            slot.receivers.push(ReceiverEntry { label, queue: queue.clone(), keyset: keyset.clone() });
        }

        Ok(Box::new(MuxEndpoint {
            local: mux.addr,
            role: mux.role,
            mux: mux.clone(),
            queue,
            keyset,
            caller_registered: AtomicBool::new(false),
            queue_max: mux.queue_max,
        }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        Vec::new()
    }
    fn transit_delay_ms(&self) -> Option<u64> {
        None
    }
    fn in_flight(&self) -> i64 {
        0
    }
    fn bump_in_flight(&self, _delta: i64) {}
    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        Vec::new()
    }
    async fn await_in_flight(&self, _timeout: Duration) {}
}

/// One call leg's endpoint: an inbox fed by the shared dispatcher + the shared
/// send socket. Deregisters on `Drop`.
struct MuxEndpoint {
    local: SocketAddr,
    role: Role,
    mux: Arc<MuxSocket>,
    queue: Arc<PacketQueue>,
    keyset: Arc<Mutex<Vec<Key>>>,
    caller_registered: AtomicBool,
    queue_max: usize,
}

#[async_trait]
impl UdpEndpoint for MuxEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        // A caller learns its own dialog key from its first outbound request
        // (the INVITE) and registers it so responses/in-dialog demux back.
        if self.role == Role::Caller && !self.caller_registered.load(Ordering::Relaxed) {
            if let Some(cid) = call_id(buf) {
                let mut g = self.mux.reg.lock().unwrap();
                g.by_call_id.insert(cid.clone(), self.queue.clone());
                self.keyset.lock().unwrap().push(Key::CallId(cid));
                self.caller_registered.store(true, Ordering::Relaxed);
            }
        }
        self.mux
            .socket
            .send_to(buf, dst)
            .await
            .map(|_| ())
            .map_err(|e| SendError { message: e.to_string() })
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.queue.take().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.queue.poll()
    }
    fn local_addr(&self) -> SocketAddr {
        self.local
    }
    fn queue_depth(&self) -> usize {
        self.queue.depth()
    }
    fn queue_max(&self) -> usize {
        self.queue_max
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

impl Drop for MuxEndpoint {
    fn drop(&mut self) {
        let mut g = self.mux.reg.lock().unwrap();
        for key in self.keyset.lock().unwrap().drain(..) {
            match key {
                Key::CallId(c) => {
                    g.by_call_id.remove(&c);
                }
                // Remove only THIS receiver from a possibly-shared slot; drop the
                // slot once its last receiver leaves.
                Key::Token { token, label } => {
                    if let Some(slot) = g.by_token.get_mut(&token) {
                        slot.receivers.retain(|r| r.label != label);
                        if slot.receivers.is_empty() {
                            g.by_token.remove(&token);
                        }
                    }
                }
            }
        }
        self.queue.close();
    }
}

/// One socket's dispatcher: parse the routing key, deliver or count-and-drop.
async fn dispatch_loop(mux: std::sync::Weak<MuxSocket>, socket: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65_536];
    while let Ok((n, src)) = socket.recv_from(&mut buf).await {
        let Some(mux) = mux.upgrade() else { return };
        let raw = &buf[..n];
        route(&mux, raw, src);
    }
}

fn route(mux: &MuxSocket, raw: &[u8], src: SocketAddr) {
    let pkt = || UdpPacket { raw: raw.to_vec(), src, arrival_ms: now_ms() };
    let cid = call_id(raw);
    let mut g = mux.reg.lock().unwrap();

    // 1. Known dialog (Call-ID we minted or already promoted).
    if let Some(cid) = &cid {
        if let Some(q) = g.by_call_id.get(cid) {
            deliver(&mux.stats, q, pkt());
            return;
        }
    }
    // 2. A new leg of a known call: an INITIAL INVITE whose Call-ID we have not
    //    seen, carrying our per-call token. Only an initial INVITE may spawn a
    //    leg (an in-dialog request with an unknown Call-ID is a stray, never a
    //    new dialog). The token entry is NON-consuming: it persists for the
    //    call's lifetime so re-routes / multi-REFER / re-REFER (further legs of
    //    the same call on this socket) each promote their own dialog.
    if is_initial_invite(raw) {
        let Some(tok) = mux.correlation.token(raw) else {
            mux.stats.orphan(OrphanReason::NoHeader, raw);
            return;
        };
        let Some(slot) = g.by_token.get_mut(&tok) else {
            mux.stats.orphan(OrphanReason::UnknownToken, raw);
            return;
        };
        // Receiver selection: the single-receiver socket delivers directly; a
        // shared socket asks the scenario-owned picker (handed the parsed leg).
        // The mux itself reads nothing but the call token — leg routing is the
        // scenario's to own.
        let idx = match slot.receivers.len() {
            0 => {
                mux.stats.orphan(OrphanReason::NoRoute, raw);
                return;
            }
            1 => 0,
            _ => match &slot.picker {
                Some(pick) => {
                    // The picker runs while we hold `mux.reg`; isolate it under
                    // `catch_unwind` so a panicking scenario callback degrades to
                    // a `no_route` orphan instead of POISONING the socket's
                    // registry mutex (which would cascade every subsequent
                    // route/bind/drop on this endpoint). It must also not re-enter
                    // the mux (it would self-deadlock on `reg`).
                    let picked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        pick(&LegInfo { raw })
                    }));
                    match picked.ok().and_then(|want| slot.receivers.iter().position(|r| r.label == want)) {
                        Some(i) => i,
                        None => {
                            mux.stats.orphan(OrphanReason::NoRoute, raw);
                            return;
                        }
                    }
                }
                None => {
                    // Several receivers but no picker to disambiguate — a
                    // scenario bug, not a silent first-wins.
                    mux.stats.orphan(OrphanReason::NoRoute, raw);
                    return;
                }
            },
        };
        slot.arrived = true;
        let queue = slot.receivers[idx].queue.clone();
        if let Some(cid) = &cid {
            slot.receivers[idx].keyset.lock().unwrap().push(Key::CallId(cid.clone()));
            g.by_call_id.insert(cid.clone(), queue.clone());
        }
        deliver(&mux.stats, &queue, pkt());
        return;
    }
    // 3. Not a known dialog and not an initial INVITE → a late straggler.
    mux.stats.orphan(OrphanReason::Stray, raw);
}

fn deliver(stats: &MuxStats, q: &PacketQueue, pkt: UdpPacket) {
    if q.offer(pkt) {
        stats.delivered.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.inbox_drop.fetch_add(1, Ordering::Relaxed);
    }
}

/// Periodic sweep of pending callee legs whose INVITE never arrived. A slot that
/// has seen at least one leg (`arrived`) is left alone — a live call may outlast
/// the pending deadline; its receivers are released on agent `Drop`, not here.
async fn reap_loop(sockets: Vec<Arc<MuxSocket>>, stats: Arc<MuxStats>, ttl: Duration) {
    let mut tick = tokio::time::interval(ttl.max(Duration::from_secs(5)));
    loop {
        tick.tick().await;
        let now = Instant::now();
        for mux in &sockets {
            let mut g = mux.reg.lock().unwrap();
            let expired: Vec<String> = g
                .by_token
                .iter()
                .filter(|(_, s)| !s.arrived && s.deadline <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                if let Some(s) = g.by_token.remove(&k) {
                    for r in &s.receivers {
                        r.queue.close();
                    }
                    stats.pending_expired.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal SIP scanners (ASCII headers; cheap, no full parse on the hot path)
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn as_str(raw: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

fn first_line(raw: &[u8]) -> String {
    as_str(raw).lines().next().unwrap_or("").trim().to_string()
}

/// The Request-URI (2nd token of the request line), or `None` for a response.
/// Used only by the scenario-facing [`LegInfo`] (never by the mux's own demux).
fn ruri(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None; // response
    }
    line.split_whitespace().nth(1).map(str::to_string)
}

/// The user-part of a SIP URI (handles `<sip:user@host>`, `sip:user@host`,
/// name-addr with a display name). For [`LegInfo`] picker helpers only.
fn uri_user(value: &str) -> Option<String> {
    let v = value.trim();
    let inner = match (v.find('<'), v.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => &v[a + 1..b],
        _ => v,
    };
    let no_scheme = inner
        .strip_prefix("sips:")
        .or_else(|| inner.strip_prefix("sip:"))
        .unwrap_or(inner);
    let user = no_scheme.split('@').next()?;
    if user.is_empty() || user.contains(' ') {
        None
    } else {
        Some(user.to_string())
    }
}

fn is_initial_invite(raw: &[u8]) -> bool {
    first_line(raw).split_whitespace().next() == Some("INVITE")
}

/// Value of header `name` (case-insensitive), scanning the header block only.
fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if h.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn call_id(raw: &[u8]) -> Option<String> {
    header_value(raw, "call-id").or_else(|| header_value(raw, "i"))
}
