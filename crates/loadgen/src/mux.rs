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
//! # Correlation (SUT-agnostic)
//!
//! Each call carries a random token. Demux precedence per inbound datagram:
//! 1. **known Call-ID** — our UAC dialog (Call-ID sniffed from our outbound
//!    INVITE), or a UAS dialog *after* its first request (the SUT-minted Call-ID
//!    we learned from the INVITE).
//! 2. **correlation token** — an inbound *initial* request whose Call-ID we've
//!    never seen, matched against the pending-UAS registry. The token travels via
//!    a pluggable [`Correlation`] source: a transparent `X-Loadgen-Id` **header**
//!    (the generic requirement — proxies forward it), or the **To URI user-part**
//!    (preserved across a B2BUA that strips custom headers, e.g. ours).
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

/// One place a correlation token can live in a callee's first request.
#[derive(Debug, Clone)]
pub enum Source {
    /// A transparent header (e.g. `X-Loadgen-Id`) — the generic requirement; a
    /// proxy forwards it unchanged.
    Header(String),
    /// The To URI user-part — preserved across a B2BUA that strips custom headers
    /// (our b2bua keeps the a-leg To URI on the b-leg).
    ToUser,
    /// The Request-URI user-part — the channel a REFER's Refer-To survives as on
    /// the transfer-target INVITE.
    RuriUser,
}

impl Source {
    fn extract(&self, raw: &[u8]) -> Option<String> {
        match self {
            Source::Header(name) => header_value(raw, name),
            Source::ToUser => header_value(raw, "to")
                .or_else(|| header_value(raw, "t"))
                .as_deref()
                .and_then(uri_user),
            Source::RuriUser => ruri(raw).and_then(|u| uri_user(&u)),
        }
    }
}

/// How the per-call correlation token is carried through the SUT — an ordered
/// list of [`Source`]s, tried in turn (so one config covers a callee leg whose
/// token rides the To and a transfer leg whose token rides the Request-URI).
#[derive(Debug, Clone)]
pub struct Correlation {
    sources: Vec<Source>,
}

impl Correlation {
    /// A single transparent header (generic SUTs; proxies forward it).
    pub fn header(name: impl Into<String>) -> Self {
        Self { sources: vec![Source::Header(name.into())] }
    }
    /// Our B2BUA: it preserves the To URI on the b-leg and the Refer-To as the
    /// transfer INVITE's Request-URI, so try both (no SUT change needed).
    pub fn b2bua() -> Self {
        Self { sources: vec![Source::ToUser, Source::RuriUser] }
    }
    /// Custom source list.
    pub fn sources(sources: Vec<Source>) -> Self {
        Self { sources }
    }
    /// The channel a scenario should embed the token in (the first source).
    pub fn primary(&self) -> &Source {
        self.sources.first().expect("Correlation needs at least one source")
    }
    /// Every candidate token found in `raw` across the configured sources.
    fn candidates(&self, raw: &[u8]) -> Vec<String> {
        self.sources.iter().filter_map(|s| s.extract(raw)).collect()
    }
}

/// A registry key owned by one endpoint (removed on its `Drop`).
#[derive(Debug, Clone)]
enum Key {
    CallId(String),
    Token(String),
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
            OrphanReason::UnknownToken => self.orphan_unknown_token.fetch_add(1, Ordering::Relaxed),
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
    /// An unknown Call-ID that is not an initial INVITE (a late straggler).
    Stray,
}
impl OrphanReason {
    fn label(self) -> &'static str {
        match self {
            OrphanReason::NoHeader => "no_header",
            OrphanReason::UnknownToken => "unknown_token",
            OrphanReason::Stray => "stray",
        }
    }
}

struct PendingEntry {
    queue: Arc<PacketQueue>,
    keyset: Arc<Mutex<Vec<Key>>>,
    deadline: Instant,
}

#[derive(Default)]
struct SocketRegistry {
    by_call_id: HashMap<String, Arc<PacketQueue>>,
    by_token: HashMap<String, PendingEntry>,
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
    /// in-flight calls).
    pub fn registry_size(&self) -> usize {
        self.endpoints
            .values()
            .map(|e| {
                let g = e.reg.lock().unwrap();
                g.by_call_id.len() + g.by_token.len()
            })
            .sum()
    }

    /// Build a per-call [`SignalingNetwork`] view: `legs` maps each **callee**
    /// endpoint address to that leg's correlation token (caller endpoints need no
    /// token).
    pub fn network(self: &Arc<Self>, legs: HashMap<SocketAddr, String>) -> MuxNetwork {
        MuxNetwork { core: self.clone(), legs }
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

/// Per-call `SignalingNetwork` over the shared [`MuxCore`].
pub struct MuxNetwork {
    core: Arc<MuxCore>,
    legs: HashMap<SocketAddr, String>,
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
        let token = self.legs.get(&opts.addr).cloned();

        if mux.role == Role::Callee {
            let token = token.clone().ok_or_else(|| BindError {
                reason: BindErrorReason::OsError,
                addr: opts.addr,
                message: format!("callee endpoint {} bound without a correlation token", opts.addr),
            })?;
            keyset.lock().unwrap().push(Key::Token(token.clone()));
            mux.reg.lock().unwrap().by_token.insert(
                token,
                PendingEntry {
                    queue: queue.clone(),
                    keyset: keyset.clone(),
                    // The callee leg should arrive within the call's recv window;
                    // a generous multiple guards against a slow SUT while still
                    // reaping a leg that never comes.
                    deadline: Instant::now() + self.core.pending_ttl * 4,
                },
            );
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
                Key::Token(t) => {
                    g.by_token.remove(&t);
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

    // 1. Known dialog.
    if let Some(cid) = &cid {
        if let Some(q) = g.by_call_id.get(cid) {
            deliver(&mux.stats, q, pkt());
            return;
        }
    }
    // 2. Correlation token → a pending callee leg (try each candidate source).
    let candidates = mux.correlation.candidates(raw);
    if !candidates.is_empty() {
        for tok in &candidates {
            if let Some(entry) = g.by_token.remove(tok) {
                if let Some(cid) = &cid {
                    g.by_call_id.insert(cid.clone(), entry.queue.clone());
                    entry.keyset.lock().unwrap().push(Key::CallId(cid.clone()));
                }
                deliver(&mux.stats, &entry.queue, pkt());
                return;
            }
        }
        // A token was present but matched no pending call.
        mux.stats.orphan(OrphanReason::UnknownToken, raw);
        return;
    }
    // 3. Uncorrelatable (no token at all).
    let reason = if is_initial_invite(raw) {
        OrphanReason::NoHeader
    } else {
        OrphanReason::Stray
    };
    mux.stats.orphan(reason, raw);
}

fn deliver(stats: &MuxStats, q: &PacketQueue, pkt: UdpPacket) {
    if q.offer(pkt) {
        stats.delivered.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.inbox_drop.fetch_add(1, Ordering::Relaxed);
    }
}

/// Periodic sweep of pending callee legs whose INVITE never arrived.
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
                .filter(|(_, e)| e.deadline <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                if let Some(e) = g.by_token.remove(&k) {
                    e.queue.close();
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
fn ruri(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None; // response
    }
    line.split_whitespace().nth(1).map(str::to_string)
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

/// The user-part of a SIP URI (handles `<sip:user@host>`, `sip:user@host`,
/// name-addr with a display name).
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

fn call_id(raw: &[u8]) -> Option<String> {
    header_value(raw, "call-id").or_else(|| header_value(raw, "i"))
}
