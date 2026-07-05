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
//! # Correlation (pluggable strategy, per run)
//!
//! Each call carries one random token; HOW the token travels through the SUT is
//! a per-run [`Correlation`] strategy with two halves — **stamp** (how the
//! scenario writes it into the outgoing INVITE; see
//! [`CorrelationStamp`]) and **extract** (how the mux recovers it from a
//! received leg). Two strategies ship: a **relayed header** (templated value in
//! e.g. `X-Loadgen-Id`, requires the SUT to relay the header) and **To-user**
//! (the token IS the To user-part — survives any SIP-correct B2BUA with zero
//! SUT cooperation). Demux precedence per inbound datagram:
//! 1. **known Call-ID** — our UAC dialog (Call-ID sniffed from our outbound
//!    INVITE), or a UAS dialog *after* its first request (the SUT-minted Call-ID
//!    we learned from the INVITE).
//! 2. **correlation token** — an inbound *initial* request whose Call-ID we've
//!    never seen, matched against the pending-UAS registry by the token the
//!    strategy extracts. The SUT carries the token unchanged onto **every**
//!    originated leg (header relay, or the copied To URI), so a call's callee
//!    (bob) and transfer-target (charlie) legs share one token.
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use scenario_harness::realcall::CorrelationStamp;
use sip_clock::Clock;
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

/// How the per-call correlation token is carried through the SUT — the per-run
/// pluggable strategy, with two halves:
/// - **stamp** ([`Correlation::stamp`]): how the token is written into the
///   outgoing INVITE (applied inside `CallEnv::outgoing_invite`, the per-call
///   identity half — orthogonal to the egress rewrite).
/// - **extract** (private `token()`, used by the demux `route` path): how the
///   token is recovered from a received leg.
///
/// Strategies:
/// - [`Correlation::header`] / [`Correlation::header_templated`] — the token
///   rides a single transparent header (e.g. `X-Loadgen-Id`) the SUT RELAYS
///   onto every originated leg (our b2bua: `B2BUA_RELAY_HEADERS`). The value
///   shape is templated (`${token}` placeholder) so the token can ride
///   structured headers (`"${token};encoding=hex"` for User-to-User,
///   `"icid-value=${token}"` for P-Charging-Vector); extraction is a regex
///   whose FIRST capture group is the token (derived from the template, or
///   overridden).
/// - [`Correlation::to_user`] — the token IS the To-header user-part. A
///   SIP-correct B2BUA copies the To URI onto its originated leg, so this
///   survives a third-party SUT that strips unknown headers (zero cooperation).
#[derive(Debug, Clone)]
pub struct Correlation {
    strategy: Strategy,
}

#[derive(Debug, Clone)]
enum Strategy {
    Header {
        name: String,
        /// Header VALUE template with a `${token}` placeholder.
        template: String,
        /// Extraction regex (first capture group = the token). `None` only for
        /// the untemplated `"${token}"` default → the whole (trimmed) header
        /// value is the token, byte-for-byte the historic behaviour.
        extract: Option<Regex>,
    },
    ToUser,
}

/// What a token looks like inside a structured header value when deriving the
/// extraction regex from a template: unreserved URI characters (covers the
/// minted `lg<uuid-simple>` tokens and any hex/uuid/alnum token).
const TOKEN_PATTERN: &str = "[A-Za-z0-9._~-]+";

impl Correlation {
    /// A single transparent correlation header carrying the bare token
    /// (byte-for-byte the historic default: stamp = the token itself, extract =
    /// the whole header value).
    pub fn header(name: impl Into<String>) -> Self {
        Self {
            strategy: Strategy::Header {
                name: name.into(),
                template: "${token}".to_string(),
                extract: None,
            },
        }
    }

    /// A relayed correlation header with a templated VALUE: `template` must
    /// contain a `${token}` placeholder (e.g. `"${token};encoding=hex"`,
    /// `"icid-value=${token}"`). `extract` optionally overrides the extraction
    /// regex — its FIRST capture group is the token; when `None` the regex is
    /// derived from the template (literal parts escaped, the placeholder
    /// replaced by an unreserved-chars capture group). Errors on a template
    /// without the placeholder, an invalid regex, or an override with no
    /// capture group.
    pub fn header_templated(
        name: impl Into<String>,
        template: impl Into<String>,
        extract: Option<&str>,
    ) -> Result<Self, String> {
        let template = template.into();
        let Some((prefix, suffix)) = template.split_once("${token}") else {
            return Err(format!(
                "correlation template {template:?} has no ${{token}} placeholder"
            ));
        };
        let extract = match extract {
            Some(re) => {
                let re = Regex::new(re).map_err(|e| format!("bad correlation extract regex: {e}"))?;
                if re.captures_len() < 2 {
                    return Err(format!(
                        "correlation extract regex {:?} needs a capture group (group 1 = the token)",
                        re.as_str()
                    ));
                }
                Some(re)
            }
            // Plain "${token}" → whole-value extraction (historic behaviour,
            // no charset assumption on the token).
            None if prefix.is_empty() && suffix.is_empty() => None,
            None => Some(
                Regex::new(&format!(
                    "{}({TOKEN_PATTERN}){}",
                    regex::escape(prefix),
                    regex::escape(suffix)
                ))
                .expect("derived correlation regex is always valid"),
            ),
        };
        Ok(Self { strategy: Strategy::Header { name: name.into(), template, extract } })
    }

    /// Token embedded as the To-header user-part — zero SUT cooperation needed.
    pub fn to_user() -> Self {
        Self { strategy: Strategy::ToUser }
    }

    /// The STAMP half: how a scenario writes `token` into the outgoing INVITE.
    pub fn stamp(&self, token: &str) -> CorrelationStamp {
        match &self.strategy {
            Strategy::Header { name, template, .. } => CorrelationStamp::Header {
                name: name.clone(),
                value: template.replace("${token}", token),
            },
            Strategy::ToUser => CorrelationStamp::ToUser,
        }
    }

    /// The EXTRACT half: the correlation token carried by `raw`, if present.
    fn token(&self, raw: &[u8]) -> Option<String> {
        match &self.strategy {
            Strategy::Header { name, extract, .. } => {
                let value = header_value(raw, name)?;
                match extract {
                    None => Some(value),
                    Some(re) => re.captures(&value)?.get(1).map(|m| m.as_str().to_string()),
                }
            }
            Strategy::ToUser => LegInfo { raw }.to_user(),
        }
    }
}

/// A simulated packet-loss model for one call's mux endpoint. Each datagram
/// (outbound on `send_to`, inbound on `recv`/`try_recv`) is independently dropped
/// with probability `rate` — a per-call knob so a scenario can be stress-tested
/// against a lossy fabric (default 1/1000 when enabled, so P(3 consecutive
/// drops)=1e-9). `rate <= 0.0` disables it: `drops()` short-circuits with no RNG
/// churn, so an un-tuned call pays nothing. The RNG is a per-endpoint xorshift
/// seeded off the call seed, advanced with a relaxed atomic (a rare same-value
/// race under concurrent send/recv is statistically irrelevant for a loss model).
struct DropModel {
    rate: f64,
    state: AtomicU64,
}

impl DropModel {
    fn new(rate: f64, seed: u64) -> Self {
        Self { rate, state: AtomicU64::new(seed | 1) }
    }
    /// Whether THIS datagram is dropped. `false` (no RNG advance) when disabled.
    fn drops(&self) -> bool {
        if self.rate <= 0.0 {
            return false;
        }
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);
        (x as f64 / u64::MAX as f64) < self.rate
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

/// A ready-made prefix-matching [`LegPicker`] — the second demux tier for the
/// several callee legs of ONE call that share a single socket (bob1 / bob2 /
/// charlie, "distinguished by prefix").
///
/// Demux is two orthogonal tiers:
/// 1. **which call instance** a leg belongs to is the correlation token (the
///    random per-call `X-Loadgen-Id`, or the To-user). The mux matches it
///    (`by_token`) BEFORE it ever consults a picker, so a picker only ever sees
///    the handful of legs of ONE instance.
/// 2. **which leg** within that instance is THIS picker's job: it returns the
///    receiver whose label is the **longest prefix of the leg's Request-URI
///    user-part**. The egress addresses each callee by role (`sip:bob2@…`,
///    `sip:charlie@…`), so the user-part names the leg; a per-call suffix
///    (`bob2-<tag>`) still routes by its label prefix, and longest-match keeps
///    `bob` vs `bob2` (or `bob1` vs `bob10`) unambiguous.
///
/// A leg whose R-URI user prefixes NONE of `labels` yields `""` — the mux counts
/// it a `no_route` orphan (observable, never mis-delivered).
pub fn prefix_leg_picker(labels: impl IntoIterator<Item = impl Into<String>>) -> LegPicker {
    let labels: Vec<String> = labels.into_iter().map(Into::into).collect();
    Arc::new(move |leg: &LegInfo| {
        let user = leg.ruri_user().unwrap_or_default();
        labels
            .iter()
            .filter(|label| user.starts_with(label.as_str()))
            .max_by_key(|label| label.len())
            .cloned()
            .unwrap_or_default()
    })
}

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
    /// Datagrams the per-call [`DropModel`] deliberately discarded, split by
    /// direction — the simulated packet loss the SUT + auto-retransmit are tested
    /// against. `out` = never hit the wire (dropped in `send_to`); `in` =
    /// delivered to the inbox but discarded on `recv` (models a network loss just
    /// before the app reads it).
    pub dropped_out: AtomicU64,
    pub dropped_in: AtomicU64,
    sample_cap: usize,
    samples: Mutex<Vec<String>>,
    /// Per-`(reason, CSeq-method)` orphan breakdown. A post-failover orphan burst
    /// is the SUT stray-sending in-dialog requests (a reclaim BYE, a re-armed
    /// keepalive OPTIONS) for a call the generator already finished; splitting the
    /// count by the CSeq method lets the NEXT reboot be triaged from `/metrics`
    /// alone — "stray BYE: N, stray OPTIONS: M" — without a packet capture. Off the
    /// hot path (orphans only, never a delivered datagram), so a `Mutex<map>` is fine.
    orphan_by_method: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
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
        let method = cseq_method(raw);
        *self.orphan_by_method.lock().unwrap().entry((reason.label(), method)).or_default() += 1;
        let mut g = self.samples.lock().unwrap();
        if g.len() < self.sample_cap {
            // Lead the sample with the CSeq (method + number) so a sampled orphan is
            // self-describing for troubleshooting, then the request/response line.
            g.push(format!("[{}] {} | {}", reason.label(), cseq_value(raw), first_line(raw)));
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

/// Everything the inbound [`route`] path needs to hand a datagram to one call: its
/// inbox, its simulated-loss model (applied above the retransmit engine), and its
/// optional retransmit engine (present iff `--auto-retransmit` is on for the call).
#[derive(Clone)]
struct Delivery {
    queue: Arc<PacketQueue>,
    drop: Arc<DropModel>,
    txns: Option<Arc<CallTxns>>,
}

/// One receiver bound on a socket: an inbox + its label (the agent name a
/// [`LegPicker`] returns) + the keyset its `Drop` drains + the loss/retransmit
/// state the inbound path applies once the leg arrives.
struct ReceiverEntry {
    label: String,
    queue: Arc<PacketQueue>,
    keyset: Arc<Mutex<Vec<Key>>>,
    drop: Arc<DropModel>,
    txns: Option<Arc<CallTxns>>,
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
    by_call_id: HashMap<String, Delivery>,
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
    /// The shared loadgen clock — stamps inbound `UdpPacket::arrival_ms` (ordering
    /// hint only; `seq` is the authority) so the mux reads no raw `SystemTime`.
    clock: Clock,
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
        clock: Clock,
    ) -> std::io::Result<Arc<Self>> {
        let stats = Arc::new(MuxStats::new(orphan_sample_cap));
        let mut endpoints = HashMap::new();
        for spec in specs {
            let socket = Arc::new(UdpSocket::bind(spec.addr).await?);
            let local = socket.local_addr()?;
            let clock = clock.clone();
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
                    clock,
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
    /// No simulated packet loss and no auto-retransmit (the historic behaviour).
    pub fn network(self: &Arc<Self>, routing: CallRouting) -> MuxNetwork {
        self.network_tuned(routing, 0.0, false, 0)
    }

    /// [`network`](Self::network) with per-call robustness knobs:
    /// - `drop_rate` (0 = off): each datagram this call's endpoints send/receive is
    ///   independently dropped, so a scenario is exercised against a lossy fabric.
    /// - `retransmit`: when set, the mux runs a per-call SIP transaction engine
    ///   that retransmits lost signaling on real timers (Timer A/E for requests,
    ///   Timer G 2xx-until-ACK for answers) and absorbs the resulting duplicates,
    ///   so a rare drop is recovered instead of failing the call.
    ///
    /// `seed` seeds each endpoint's loss RNG (derive it from the per-call id seed
    /// for reproducibility).
    pub fn network_tuned(
        self: &Arc<Self>,
        routing: CallRouting,
        drop_rate: f64,
        retransmit: bool,
        seed: u64,
    ) -> MuxNetwork {
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
            drop_rate,
            retransmit,
            drop_seed: AtomicU64::new(seed | 1),
        }
    }

    /// Render the mux Prometheus series.
    pub fn render_prometheus(&self) -> String {
        let s = &self.stats;
        let mut out = String::new();
        // Orphans are labelled by reason AND CSeq method (`sum by(reason)` still
        // aggregates to the per-reason total for existing queries). Always emit the
        // three reason×none zero-series so a fresh run has the series present.
        out.push_str("# HELP loadgen_mux_orphan_total Inbound datagrams that matched no call, by reason and CSeq method.\n");
        out.push_str("# TYPE loadgen_mux_orphan_total counter\n");
        let by = s.orphan_by_method.lock().unwrap();
        if by.is_empty() {
            for r in ["no_header", "unknown_token", "stray"] {
                out.push_str(&format!("loadgen_mux_orphan_total{{reason=\"{r}\",method=\"none\"}} 0\n"));
            }
        } else {
            for ((reason, method), n) in by.iter() {
                out.push_str(&format!(
                    "loadgen_mux_orphan_total{{reason=\"{reason}\",method=\"{method}\"}} {n}\n"
                ));
            }
        }
        drop(by);
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
        out.push_str("# HELP loadgen_drop_total Datagrams dropped by the simulated packet-loss model, by direction.\n");
        out.push_str("# TYPE loadgen_drop_total counter\n");
        out.push_str(&format!(
            "loadgen_drop_total{{dir=\"out\"}} {}\n",
            s.dropped_out.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "loadgen_drop_total{{dir=\"in\"}} {}\n",
            s.dropped_in.load(Ordering::Relaxed)
        ));
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
    /// Per-call simulated packet-loss rate applied to every endpoint bound on
    /// this network (0 = off). Each endpoint gets its own RNG seeded off
    /// `drop_seed` so alice/bob/charlie drop independently.
    drop_rate: f64,
    /// Whether each endpoint runs the per-call SIP retransmit engine.
    retransmit: bool,
    drop_seed: AtomicU64,
}

impl MuxNetwork {
    /// The next per-endpoint loss RNG seed (golden-ratio stride so alice/bob/
    /// charlie of the same call get well-separated, non-zero seeds).
    fn next_drop_seed(&self) -> u64 {
        self.drop_seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
    }
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
        // One loss model + (optional) retransmit engine per endpoint, shared
        // between this endpoint (outbound) and the registry entry the inbound
        // `route` path consults, so both directions and the resend tasks agree.
        let drop = Arc::new(DropModel::new(self.drop_rate, self.next_drop_seed()));
        let txns = self
            .retransmit
            .then(|| Arc::new(CallTxns::new(mux.socket.clone(), drop.clone(), mux.stats.clone())));

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
            slot.receivers.push(ReceiverEntry {
                label,
                queue: queue.clone(),
                keyset: keyset.clone(),
                drop: drop.clone(),
                txns: txns.clone(),
            });
        }

        Ok(Box::new(MuxEndpoint {
            local: mux.addr,
            role: mux.role,
            mux: mux.clone(),
            queue,
            keyset,
            caller_registered: AtomicBool::new(false),
            queue_max: mux.queue_max,
            drop,
            txns,
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
    /// Simulated per-call packet loss (disabled by default), shared with this
    /// call's [`CallTxns`] so retransmits are lossy too. Outbound loss is applied
    /// here in `send_to`; inbound loss is applied in [`route`] (above the retransmit
    /// engine, so a lost inbound datagram is truly gone and the peer's retransmit
    /// re-delivers it fresh).
    drop: Arc<DropModel>,
    /// Per-call SIP retransmit engine (present only when `--auto-retransmit` is on
    /// for this call). Records outbound requests/answers and drives their timers.
    txns: Option<Arc<CallTxns>>,
}

#[async_trait]
impl UdpEndpoint for MuxEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        // A caller learns its own dialog key from its first outbound request
        // (the INVITE) and registers it so responses/in-dialog demux back. This is
        // local bookkeeping, so it happens even when the datagram is then dropped
        // on the wire below (a real UAC that loses its INVITE still owns the dialog).
        if self.role == Role::Caller && !self.caller_registered.load(Ordering::Relaxed) {
            if let Some(cid) = call_id(buf) {
                let mut g = self.mux.reg.lock().unwrap();
                g.by_call_id.insert(
                    cid.clone(),
                    Delivery {
                        queue: self.queue.clone(),
                        drop: self.drop.clone(),
                        txns: self.txns.clone(),
                    },
                );
                self.keyset.lock().unwrap().push(Key::CallId(cid));
                self.caller_registered.store(true, Ordering::Relaxed);
            }
        }
        // Record the outbound message in the retransmit engine BEFORE the loss
        // check: a dropped request is exactly what the resender must recover, and
        // the resender re-applies the same loss model on every retry.
        if let Some(txns) = &self.txns {
            txns.on_outbound(buf, dst);
        }
        // Simulated loss: report success (the txn believes it sent) but never put
        // the datagram on the wire — the SUT never sees it, so only auto-retransmit
        // recovers the call.
        if self.drop.drops() {
            self.mux.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            return Ok(());
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
        drop(g);
        self.queue.close();
        // Stop this call's resender tasks now (call ended) rather than letting them
        // run to their transaction timeout — bounded task lifetime under load.
        if let Some(txns) = &self.txns {
            txns.shutdown();
        }
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
    let cid = call_id(raw);
    let mut g = mux.reg.lock().unwrap();

    // 1. Known dialog (Call-ID we minted or already promoted). Clone the
    //    `Delivery` out and RELEASE the registry lock before running the loss
    //    check + retransmit engine (which may send on the socket) — never hold
    //    `reg` across a send.
    if let Some(cid) = &cid {
        if let Some(d) = g.by_call_id.get(cid) {
            let d = d.clone();
            drop(g);
            handle_inbound(mux, &d, raw, src);
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
        // Snapshot this receiver's delivery state, then (under the same lock)
        // promote token→Call-ID so in-dialog traffic demuxes directly.
        let delivery = {
            let recv = &slot.receivers[idx];
            Delivery { queue: recv.queue.clone(), drop: recv.drop.clone(), txns: recv.txns.clone() }
        };
        if let Some(cid) = &cid {
            slot.receivers[idx].keyset.lock().unwrap().push(Key::CallId(cid.clone()));
            g.by_call_id.insert(cid.clone(), delivery.clone());
        }
        drop(g);
        handle_inbound(mux, &delivery, raw, src);
        return;
    }
    // 3. Not a known dialog and not an initial INVITE → a late straggler.
    mux.stats.orphan(OrphanReason::Stray, raw);
}

/// Hand one resolved-to-a-call datagram to the app: apply simulated inbound loss
/// (above the retransmit engine, so a lost datagram is truly gone and the peer's
/// retransmit re-delivers it fresh), then let the retransmit engine dedup /
/// stop-resenders / re-answer; a datagram the engine ABSORBS (a duplicate the app
/// must not see) is not enqueued.
fn handle_inbound(mux: &MuxSocket, d: &Delivery, raw: &[u8], src: SocketAddr) {
    if d.drop.drops() {
        mux.stats.dropped_in.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if let Some(txns) = &d.txns {
        if !txns.on_inbound(raw, src) {
            return; // duplicate absorbed (engine did any re-ACK / re-answer)
        }
    }
    let arrival_ms = mux.clock.now_ms().max(0) as u64;
    deliver(&mux.stats, &d.queue, UdpPacket { raw: raw.to_vec(), src, arrival_ms });
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

fn as_str(raw: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

fn first_line(raw: &[u8]) -> String {
    as_str(raw).lines().next().unwrap_or("").trim().to_string()
}

/// The CSeq line value (`<num> <METHOD>`) of a datagram, for an orphan sample.
/// Empty if absent. Works for requests and responses (CSeq echoes the method).
fn cseq_value(raw: &[u8]) -> String {
    for line in as_str(raw).lines() {
        let l = line.trim();
        if l.len() >= 5 && l[..5].eq_ignore_ascii_case("cseq:") {
            return format!("CSeq: {}", l[5..].trim());
        }
    }
    String::new()
}

/// The CSeq method, mapped to a BOUNDED static label so it is a safe (low-
/// cardinality) Prometheus label. `none` when absent, `other` for an unrecognized
/// method — so a stray BYE / OPTIONS burst after a reboot is visible per-method.
fn cseq_method(raw: &[u8]) -> &'static str {
    for line in as_str(raw).lines() {
        let l = line.trim();
        if l.len() >= 5 && l[..5].eq_ignore_ascii_case("cseq:") {
            let m = l[5..].split_whitespace().nth(1).unwrap_or("");
            return match m.to_ascii_uppercase().as_str() {
                "INVITE" => "INVITE",
                "ACK" => "ACK",
                "BYE" => "BYE",
                "CANCEL" => "CANCEL",
                "OPTIONS" => "OPTIONS",
                "REFER" => "REFER",
                "NOTIFY" => "NOTIFY",
                "PRACK" => "PRACK",
                "UPDATE" => "UPDATE",
                "INFO" => "INFO",
                "SUBSCRIBE" => "SUBSCRIBE",
                "MESSAGE" => "MESSAGE",
                "" => "none",
                _ => "other",
            };
        }
    }
    "none"
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
/// name-addr with a display name). A userless URI (`sip:host`) yields `None` —
/// load-bearing for the To-user correlation strategy, which must not mint a
/// bogus host-shaped token. For [`LegInfo`] helpers + token extraction.
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
    let (user, _host) = no_scheme.split_once('@')?;
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

/// Whether `raw` is a SIP response (status line) vs a request.
fn is_response(raw: &[u8]) -> bool {
    first_line(raw).starts_with("SIP/2.0")
}

/// The response status code (`200` from `SIP/2.0 200 OK`), or `None` for a request.
fn resp_status(raw: &[u8]) -> Option<u16> {
    let line = first_line(raw);
    if !line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().nth(1).and_then(|s| s.parse().ok())
}

/// The request method (`INVITE` from the request line), or `None` for a response.
fn req_method(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().next().map(str::to_string)
}

/// The CSeq sequence number (the `<num>` of `CSeq: <num> <METHOD>`).
fn cseq_num(raw: &[u8]) -> Option<u32> {
    for line in as_str(raw).lines() {
        let l = line.trim();
        if l.len() >= 5 && l[..5].eq_ignore_ascii_case("cseq:") {
            return l[5..].split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    None
}

/// Whether a `Require` header lists the `100rel` option-tag (comma-folded,
/// case-insensitive) — the reliable-provisional marker (RFC 3262 §3). `Require`
/// has no compact form.
fn require_has_100rel(raw: &[u8]) -> bool {
    header_value(raw, "require")
        .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("100rel")))
}

/// The `RSeq` value of a reliable provisional (RFC 3262 §3), or `None`.
fn rseq_of(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rseq")?.trim().parse().ok()
}

/// The RAck response-num (its FIRST token = the acknowledged 1xx's RSeq,
/// RFC 3262 §7.2) of a PRACK, or `None`.
fn rack_rseq(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rack")?.split_whitespace().next()?.parse().ok()
}

/// The `branch` parameter of the TOP-most Via header (RFC 3261 §17 transaction
/// key), or `None` if absent. Only the first Via matters — it is OUR via on a
/// request we sent and is echoed by the UAS onto the matching response.
fn via_branch(raw: &[u8]) -> Option<String> {
    for line in as_str(raw).lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        let h = h.trim();
        if h.eq_ignore_ascii_case("via") || h.eq_ignore_ascii_case("v") {
            // First Via wins (topmost). Extract `branch=` up to the next param sep.
            let pos = v.find("branch=")?;
            let rest = &v[pos + "branch=".len()..];
            let end = rest.find([';', ',', ' ', '\t']).unwrap_or(rest.len());
            let b = rest[..end].trim();
            return (!b.is_empty()).then(|| b.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Retransmit engine (per call, present only when --auto-retransmit is on)
// ---------------------------------------------------------------------------

/// RFC 3261 default transaction timers driving the engine.
const T1: Duration = Duration::from_millis(500); // first retransmit interval
const T2: Duration = Duration::from_secs(4); // non-INVITE / 2xx backoff cap
const TXN_TIMEOUT: Duration = Duration::from_secs(32); // Timer B/F/H = 64·T1

/// Stop-control shared between a spawned resender task and the [`CallTxns`] engine
/// that owns it. The engine flips `stop` (and wakes the task) when the transaction
/// is acknowledged or the call ends; the flag is authoritative (the wake is only a
/// latency optimisation — a lost `Notify` wake is caught by the post-sleep check).
struct ResendCtl {
    stop: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ResendCtl {
    fn new() -> Arc<Self> {
        Arc::new(Self { stop: AtomicBool::new(false), notify: tokio::sync::Notify::new() })
    }
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Spawn a resender: retransmit `bytes` to `dst` on the SIP timer schedule until
/// `ctl` is stopped or the transaction times out. `invite` selects the backoff —
/// INVITE Timer A doubles unbounded (until Timer B); non-INVITE Timer E / 2xx
/// Timer G doubles but caps at T2. Each retransmit re-applies the loss model, so a
/// retransmit can itself be dropped (the point of the robustness test).
fn spawn_resender(
    ctl: Arc<ResendCtl>,
    socket: Arc<UdpSocket>,
    drop: Arc<DropModel>,
    stats: Arc<MuxStats>,
    bytes: Vec<u8>,
    dst: SocketAddr,
    invite: bool,
) {
    tokio::spawn(async move {
        let mut interval = T1;
        let deadline = tokio::time::Instant::now() + TXN_TIMEOUT;
        loop {
            tokio::select! {
                _ = ctl.notify.notified() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            if ctl.stopped() || tokio::time::Instant::now() >= deadline {
                return;
            }
            if drop.drops() {
                stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            } else {
                let _ = socket.send_to(&bytes, dst).await;
            }
            interval = if invite {
                interval.saturating_mul(2)
            } else {
                interval.saturating_mul(2).min(T2)
            };
        }
    });
}

/// Per-call SIP transaction engine: records what the harness sends, retransmits it
/// on real timers until acknowledged, and absorbs the duplicate traffic that
/// recovery produces so the strict scripted agent never sees a retransmit.
///
/// The engine is METHOD-GENERIC: requests are keyed by their top-Via branch and
/// classified only INVITE vs non-INVITE (Timer A vs Timer E backoff), so PRACK /
/// UPDATE / INFO / any future method ride the same client-txn, reactive-re-answer
/// and duplicate-absorption paths with no per-method code.
///
/// Covered directions (the "full bidirectional" robustness contract):
/// - **our requests** (INVITE / BYE / OPTIONS / REFER / CANCEL / re-INVITE /
///   PRACK / UPDATE / …): retransmitted (Timer A/E) until the matching response
///   arrives.
/// - **our INVITE answers** (2xx): retransmitted (Timer G) until the ACK arrives —
///   recovers a lost 2xx OR a lost ACK.
/// - **our RELIABLE provisionals** (1xx with `Require: 100rel` + `RSeq`,
///   RFC 3262 §3): retransmitted until the matching PRACK (RAck response-num =
///   the 1xx's RSeq) arrives — a reliable 1xx is guaranteed-delivery, unlike the
///   best-effort plain 18x (deliberately NOT retransmitted; the driver gates its
///   delivery rate instead).
/// - **our non-INVITE answers**: re-sent reactively when the peer retransmits the
///   request (its response was lost).
/// - **our ACK to a 2xx**: re-sent when a retransmitted 2xx arrives (our ACK was lost).
/// - **inbound duplicates**: absorbed, so the scripted agent's strict `expect`
///   never chokes on a retransmit.
struct CallTxns {
    socket: Arc<UdpSocket>,
    drop: Arc<DropModel>,
    stats: Arc<MuxStats>,
    inner: Mutex<TxnInner>,
}

#[derive(Default)]
struct TxnInner {
    /// Our in-flight client transactions, keyed by the request's top-Via branch →
    /// its resender (stopped when the response arrives).
    client: HashMap<String, Arc<ResendCtl>>,
    /// Our proactive 2xx (INVITE server txn) resenders, keyed by (Call-ID, CSeq
    /// number) so the inbound ACK — which carries a *different* branch — can stop them.
    invite_2xx: HashMap<(String, u32), Arc<ResendCtl>>,
    /// Our proactive RELIABLE-1xx resenders (RFC 3262 §3: retransmit until
    /// PRACKed), keyed by (Call-ID, RSeq) so the inbound PRACK — whose RAck
    /// response-num carries that RSeq but whose branch is its own — can stop them.
    reliable_1xx: HashMap<(String, u64), Arc<ResendCtl>>,
    /// The last response we sent per server txn (request branch → bytes+dst), for a
    /// reactive re-answer when the peer retransmits the request.
    server: HashMap<String, (Vec<u8>, SocketAddr)>,
    /// ACKs we sent, keyed by (Call-ID, CSeq number), for re-ACK on a duplicate 2xx.
    acks: HashMap<(String, u32), (Vec<u8>, SocketAddr)>,
    /// Inbound `(branch, discriminator)` already delivered — duplicate detection.
    seen_in: HashSet<(String, String)>,
    /// Call ended: stop tracking and reject new resenders.
    closed: bool,
}

impl CallTxns {
    fn new(socket: Arc<UdpSocket>, drop: Arc<DropModel>, stats: Arc<MuxStats>) -> Self {
        Self { socket, drop, stats, inner: Mutex::new(TxnInner::default()) }
    }

    /// Best-effort non-blocking send for the reactive resends (re-ACK / re-answer)
    /// that run on the inbound (sync) path — re-applies the loss model.
    fn send(&self, bytes: &[u8], dst: SocketAddr) {
        if self.drop.drops() {
            self.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _ = self.socket.try_send_to(bytes, dst);
    }

    /// Record an outbound datagram and arm any retransmission it needs.
    fn on_outbound(&self, raw: &[u8], dst: SocketAddr) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        if is_response(raw) {
            let Some(branch) = via_branch(raw) else { return };
            g.server.insert(branch, (raw.to_vec(), dst));
            let status = resp_status(raw).unwrap_or(0);
            // Proactive 2xx-until-ACK for an INVITE answer (Timer G).
            if (200..300).contains(&status) && cseq_method(raw) == "INVITE" {
                if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_num(raw)) {
                    if let std::collections::hash_map::Entry::Vacant(e) = g.invite_2xx.entry((cid, cseq)) {
                        let ctl = ResendCtl::new();
                        e.insert(ctl.clone());
                        spawn_resender(
                            ctl,
                            self.socket.clone(),
                            self.drop.clone(),
                            self.stats.clone(),
                            raw.to_vec(),
                            dst,
                            false,
                        );
                    }
                }
            }
            // Proactive reliable-1xx-until-PRACK (RFC 3262 §3): a provisional we
            // send with `Require: 100rel` + `RSeq` is guaranteed-delivery — the UAS
            // retransmits it until the matching PRACK. Without this, a dropped
            // reliable 183 is unrecoverable: the peer's INVITE resender already
            // stopped on the SUT's 100 Trying, so nobody would resend anything.
            // (A plain 18x stays best-effort by design.)
            if (101..200).contains(&status)
                && cseq_method(raw) == "INVITE"
                && require_has_100rel(raw)
            {
                if let (Some(cid), Some(rseq)) = (call_id(raw), rseq_of(raw)) {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        g.reliable_1xx.entry((cid, rseq))
                    {
                        let ctl = ResendCtl::new();
                        e.insert(ctl.clone());
                        spawn_resender(
                            ctl,
                            self.socket.clone(),
                            self.drop.clone(),
                            self.stats.clone(),
                            raw.to_vec(),
                            dst,
                            false,
                        );
                    }
                }
            }
            return;
        }
        // Request.
        let method = req_method(raw).unwrap_or_default();
        if method == "ACK" {
            // ACK is not a retransmitting transaction; remember it for re-ACK.
            if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_num(raw)) {
                g.acks.insert((cid, cseq), (raw.to_vec(), dst));
            }
            return;
        }
        let Some(branch) = via_branch(raw) else { return };
        if let Some(old) = g.client.remove(&branch) {
            old.stop();
        }
        let invite = method == "INVITE";
        let ctl = ResendCtl::new();
        g.client.insert(branch, ctl.clone());
        spawn_resender(
            ctl,
            self.socket.clone(),
            self.drop.clone(),
            self.stats.clone(),
            raw.to_vec(),
            dst,
            invite,
        );
    }

    /// Process an inbound datagram. Returns `true` to deliver it to the app,
    /// `false` to ABSORB it (a duplicate the strict agent must not see — any
    /// re-ACK / re-answer has already been sent here).
    fn on_inbound(&self, raw: &[u8], _src: SocketAddr) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return true;
        }
        let branch = via_branch(raw).unwrap_or_default();
        if is_response(raw) {
            let status = resp_status(raw).unwrap_or(0);
            let is_invite = cseq_method(raw) == "INVITE";
            // Stop our client resender: INVITE Timer A stops on the FIRST provisional
            // (incl. `100 Trying`, RFC 3261 §17.1.1.2); non-INVITE only on a final.
            // We deliberately do NOT keep retransmitting the INVITE to force the UAS
            // to resend a lost 18x — a non-PRACK provisional is best-effort and may
            // be lost (the caller tolerates it via `try_expect_answer`, and the
            // driver gates the cross-call 18x delivery rate instead).
            let stop = if is_invite { status >= 100 } else { status >= 200 };
            if stop {
                if let Some(ctl) = g.client.remove(&branch) {
                    ctl.stop();
                }
            }
            let key = (branch, format!("r{status}"));
            if g.seen_in.contains(&key) {
                // Duplicate response. A retransmitted INVITE 2xx means our ACK was
                // lost → re-ACK it.
                if (200..300).contains(&status) && is_invite {
                    if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_num(raw)) {
                        if let Some((ack, dst)) = g.acks.get(&(cid, cseq)).cloned() {
                            self.send(&ack, dst);
                        }
                    }
                }
                return false;
            }
            g.seen_in.insert(key);
            return true;
        }
        // Inbound request.
        let method = req_method(raw).unwrap_or_default();
        if method == "ACK" {
            // The ACK confirms our INVITE 2xx → stop its proactive resender.
            if let (Some(cid), Some(cseq)) = (call_id(raw), cseq_num(raw)) {
                if let Some(ctl) = g.invite_2xx.remove(&(cid, cseq)) {
                    ctl.stop();
                }
            }
            let key = (branch, "qACK".to_string());
            if g.seen_in.contains(&key) {
                return false; // duplicate ACK
            }
            g.seen_in.insert(key);
            return true;
        }
        if method == "PRACK" {
            // The PRACK acknowledges our reliable 1xx (RAck response-num = its
            // RSeq, RFC 3262 §7.2) → stop that proactive resender. Idempotent, so
            // it runs before the duplicate check below.
            if let (Some(cid), Some(rseq)) = (call_id(raw), rack_rseq(raw)) {
                if let Some(ctl) = g.reliable_1xx.remove(&(cid, rseq)) {
                    ctl.stop();
                }
            }
        }
        let key = (branch.clone(), format!("q{method}"));
        if g.seen_in.contains(&key) {
            // The peer retransmitted this request → our response was lost; re-send it.
            if let Some((resp, dst)) = g.server.get(&branch).cloned() {
                self.send(&resp, dst);
            }
            return false;
        }
        g.seen_in.insert(key);
        true
    }

    /// Stop every resender and drop tracked state (call ended).
    fn shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        for (_, ctl) in g.client.drain() {
            ctl.stop();
        }
        for (_, ctl) in g.invite_2xx.drain() {
            ctl.stop();
        }
        for (_, ctl) in g.reliable_1xx.drain() {
            ctl.stop();
        }
        g.server.clear();
        g.acks.clear();
        g.seen_in.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal initial INVITE carrying the given To header + optional extra
    /// header line, for extraction tests.
    fn invite(to: &str, extra: &str) -> Vec<u8> {
        format!(
            "INVITE sip:x@127.0.0.1 SIP/2.0\r\nCall-ID: c1@h\r\n{extra}To: {to}\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
        .into_bytes()
    }

    fn stamp_header(c: &Correlation, token: &str) -> (String, String) {
        match c.stamp(token) {
            CorrelationStamp::Header { name, value } => (name, value),
            other => panic!("expected a Header stamp, got {other:?}"),
        }
    }

    /// The untuned default: plain header stamp/extract is byte-for-byte today's
    /// behaviour — stamp value == the bare token, extract == the whole (trimmed)
    /// header value, whatever its charset.
    #[test]
    fn plain_header_default_is_historic_behaviour() {
        let c = Correlation::header("X-Loadgen-Id");
        let (name, value) = stamp_header(&c, "lgdeadbeef");
        assert_eq!((name.as_str(), value.as_str()), ("X-Loadgen-Id", "lgdeadbeef"));

        let raw = invite("<sip:bob@127.0.0.1>", "X-Loadgen-Id: lgdeadbeef\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("lgdeadbeef"));

        // Whole-value semantics: a value outside the derived-token charset is
        // still returned verbatim (no regex narrowing on the untuned default).
        let odd = invite("<sip:bob@127.0.0.1>", "X-Loadgen-Id: weird/value+x\r\n");
        assert_eq!(c.token(&odd).as_deref(), Some("weird/value+x"));

        // And header_templated with the bare "${token}" template is the same.
        let c2 = Correlation::header_templated("X-Loadgen-Id", "${token}", None).unwrap();
        assert_eq!(c2.token(&odd).as_deref(), Some("weird/value+x"));
    }

    /// UUI-shaped template (RFC 7433 User-to-User): the token rides
    /// `User-to-User: <token>;encoding=hex`; the derived regex recovers it.
    #[test]
    fn uui_shaped_template_renders_and_extracts() {
        let c =
            Correlation::header_templated("User-to-User", "${token};encoding=hex", None).unwrap();
        let (name, value) = stamp_header(&c, "lg0a1b2c");
        assert_eq!((name.as_str(), value.as_str()), ("User-to-User", "lg0a1b2c;encoding=hex"));

        let raw = invite("<sip:bob@127.0.0.1>", "User-to-User: lg0a1b2c;encoding=hex\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("lg0a1b2c"));

        // A leg without the header yields no token (orphan path).
        assert_eq!(c.token(&invite("<sip:bob@127.0.0.1>", "")), None);
    }

    /// PCV-shaped template (P-Charging-Vector `icid-value=`): rendered inside a
    /// param list; the derived regex recovers the token even when the SUT
    /// appends further params after it.
    #[test]
    fn pcv_shaped_template_renders_and_extracts() {
        let c =
            Correlation::header_templated("P-Charging-Vector", "icid-value=${token}", None)
                .unwrap();
        let (name, value) = stamp_header(&c, "lgfeed01");
        assert_eq!(
            (name.as_str(), value.as_str()),
            ("P-Charging-Vector", "icid-value=lgfeed01")
        );

        let relayed = invite(
            "<sip:bob@127.0.0.1>",
            "P-Charging-Vector: icid-value=lgfeed01;icid-generated-at=10.0.0.1\r\n",
        );
        assert_eq!(c.token(&relayed).as_deref(), Some("lgfeed01"));
    }

    /// The CLI extraction override: an explicit regex (first capture group =
    /// the token) beats the derived one; invalid overrides are rejected.
    #[test]
    fn explicit_extract_override() {
        let c = Correlation::header_templated(
            "User-to-User",
            "${token};encoding=hex",
            Some(r"^\s*([0-9a-fx]+)\s*;"),
        )
        .unwrap();
        let raw = invite("<sip:bob@127.0.0.1>", "User-to-User: 0xabc ;encoding=hex\r\n");
        assert_eq!(c.token(&raw).as_deref(), Some("0xabc"));

        // No capture group → config error, not a silent mis-extraction.
        assert!(Correlation::header_templated("H", "${token}", Some("nogroup")).is_err());
        // Invalid regex → error.
        assert!(Correlation::header_templated("H", "${token}", Some("(")).is_err());
        // Template without the placeholder → error.
        assert!(Correlation::header_templated("H", "no-placeholder", None).is_err());
    }

    /// To-user strategy: stamp is [`CorrelationStamp::ToUser`]; extraction
    /// recovers the token from the To user-part (via [`LegInfo::to_user`]) in
    /// both name-addr and bare-URI shapes — no loadgen header involved.
    #[test]
    fn to_user_strategy_extracts_from_to_header() {
        let c = Correlation::to_user();
        assert!(matches!(c.stamp("lg123"), CorrelationStamp::ToUser));

        let name_addr = invite("\"Bee\" <sip:lg123abc@10.0.0.9:5070>", "");
        assert_eq!(c.token(&name_addr).as_deref(), Some("lg123abc"));

        let bare = invite("sip:lg456def@10.0.0.9", "");
        assert_eq!(c.token(&bare).as_deref(), Some("lg456def"));

        // A relayed loadgen header is IGNORED by this strategy (extract is
        // To-user only), and a userless To yields no token.
        let userless = invite("<sip:10.0.0.9:5070>", "X-Loadgen-Id: lg999\r\n");
        assert_eq!(c.token(&userless), None);
    }

    // -- prefix_leg_picker: the R-URI-prefix leg tier (bob/bob2/charlie on one
    //    socket, once the token has already picked the call instance) ----------

    /// An INVITE with a specific Request-URI (+ matching To), for the leg-picker
    /// tests — the mux hands a picker a `LegInfo` over exactly these bytes.
    fn invite_ruri(ruri: &str) -> Vec<u8> {
        format!(
            "INVITE {ruri} SIP/2.0\r\nCall-ID: c1@h\r\nTo: <{ruri}>\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
        .into_bytes()
    }

    /// The picker routes each shared-socket leg to the receiver whose label is the
    /// LONGEST prefix of the R-URI user — bob / bob2 / charlie on one port,
    /// including a per-call-suffixed user, with longest-match resolving bob-vs-bob2;
    /// an unknown user (or a response with no R-URI) is a no-route (`""`).
    #[test]
    fn prefix_leg_picker_routes_by_longest_ruri_user_prefix() {
        let pick = prefix_leg_picker(["bob", "bob2", "charlie"]);
        let route = |ruri: &str| {
            let raw = invite_ruri(ruri);
            pick(&LegInfo { raw: raw.as_slice() })
        };

        assert_eq!(route("sip:bob@10.0.0.1:5070"), "bob");
        // "bob2" is prefixed by both "bob" and "bob2" → longest wins.
        assert_eq!(route("sip:bob2@10.0.0.1:5070"), "bob2");
        assert_eq!(route("sip:charlie@10.0.0.1:5070"), "charlie");
        // A per-call suffix on the user still routes by its label prefix.
        assert_eq!(route("sip:bob2-lg99@10.0.0.1:5070"), "bob2");
        assert_eq!(route("sip:charlie.7f3a@10.0.0.1:5070"), "charlie");
        // No label prefixes the user → no route (a no_route orphan at the mux).
        assert_eq!(route("sip:dave@10.0.0.1:5070"), "");
        // A response (no Request-URI) is a no-route, never a panic.
        assert_eq!(pick(&LegInfo { raw: &b"SIP/2.0 200 OK\r\n\r\n"[..] }), "");
    }

    /// Longest-match disambiguates labels that are prefixes of each other even
    /// when the shorter also matches (`bob1` vs `bob10`).
    #[test]
    fn prefix_leg_picker_longest_match_disambiguates_numeric_siblings() {
        let pick = prefix_leg_picker(["bob1", "bob10"]);
        let route = |ruri: &str| {
            let raw = invite_ruri(ruri);
            pick(&LegInfo { raw: raw.as_slice() })
        };
        assert_eq!(route("sip:bob10@h"), "bob10");
        assert_eq!(route("sip:bob1@h"), "bob1");
        assert_eq!(route("sip:bob10x@h"), "bob10");
    }

    // -- CallTxns retransmit engine: method-generic regression --------------
    //
    // The engine must stay method-generic (Timer E for ANY non-INVITE request,
    // duplicate absorption keyed by (branch, method)) and cover the RFC 3262
    // reliable-1xx-until-PRACK server obligation. These tests drive `CallTxns`
    // directly over a loopback UDP pair on the real clock (T1 = 500 ms, so each
    // stays a few seconds).

    use std::net::SocketAddr;
    use tokio::net::UdpSocket as TokioUdp;
    use tokio::time::{timeout, Duration as TokioDuration};

    async fn txn_rig() -> (Arc<CallTxns>, TokioUdp, SocketAddr) {
        let sock = Arc::new(TokioUdp::bind("127.0.0.1:0").await.unwrap());
        let peer = TokioUdp::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let txns = Arc::new(CallTxns::new(
            sock,
            Arc::new(DropModel::new(0.0, 1)),
            Arc::new(MuxStats::new(4)),
        ));
        (txns, peer, peer_addr)
    }

    async fn recv_one(peer: &TokioUdp, window_ms: u64) -> Option<String> {
        let mut buf = vec![0u8; 2048];
        match timeout(TokioDuration::from_millis(window_ms), peer.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(String::from_utf8_lossy(&buf[..n]).to_string()),
            _ => None,
        }
    }

    fn update_req(branch: &str) -> Vec<u8> {
        format!(
            "UPDATE sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 UPDATE\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, branch: &str, cseq: &str, extra: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq}\r\n{extra}\r\n"
        )
        .into_bytes()
    }

    fn prack_req(branch: &str, rack: &str) -> Vec<u8> {
        format!(
            "PRACK sip:b@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5061;branch={branch}\r\n\
             Call-ID: ct1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\r\n"
        )
        .into_bytes()
    }

    /// Timer E is METHOD-GENERIC: an outbound UPDATE (a non-INVITE the engine has
    /// no per-method code for) is retransmitted after ~T1 and the resender stops
    /// on its final response; a duplicate of that response is then absorbed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_timer_e_is_method_generic_for_update() {
        let (txns, peer, peer_addr) = txn_rig().await;

        txns.on_outbound(&update_req("z9hG4bK-u1"), peer_addr);
        let got = recv_one(&peer, 2_000).await.expect("Timer E retransmit of the UPDATE");
        assert!(got.starts_with("UPDATE "), "retransmit is the UPDATE: {got}");

        // The 200 (UPDATE) stops the resender…
        let ok = resp(200, "z9hG4bK-u1", "2 UPDATE", "");
        assert!(txns.on_inbound(&ok, peer_addr), "first 200 (UPDATE) is delivered");
        // …and its duplicate is absorbed (method-generic dedup).
        assert!(!txns.on_inbound(&ok, peer_addr), "duplicate 200 (UPDATE) absorbed");
        assert!(
            recv_one(&peer, 1_500).await.is_none(),
            "UPDATE resender must stop on the final response"
        );
        txns.shutdown();
    }

    /// Duplicate absorption + reactive re-answer are method-generic: a
    /// retransmitted inbound PRACK is absorbed and our recorded 200 (PRACK) is
    /// re-sent (the peer's copy was evidently lost).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_absorbs_duplicate_prack_and_reanswers() {
        let (txns, peer, peer_addr) = txn_rig().await;

        let prack = prack_req("z9hG4bK-p1", "1 1 INVITE");
        assert!(txns.on_inbound(&prack, peer_addr), "first PRACK is delivered");
        txns.on_outbound(&resp(200, "z9hG4bK-p1", "2 PRACK", ""), peer_addr);

        assert!(!txns.on_inbound(&prack, peer_addr), "duplicate PRACK absorbed");
        let got = recv_one(&peer, 1_000).await.expect("reactive re-answer to the dup PRACK");
        assert!(got.starts_with("SIP/2.0 200"), "re-answer is our 200 (PRACK): {got}");
        txns.shutdown();
    }

    /// RFC 3262 §3: a RELIABLE provisional we send (Require:100rel + RSeq) is
    /// retransmitted until the matching PRACK (RAck response-num = its RSeq)
    /// arrives — the gap that made a dropped reliable 183 unrecoverable (the
    /// peer's INVITE resender already stopped on the 100 Trying). A plain 18x
    /// stays best-effort (never proactively retransmitted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calltxns_retransmits_reliable_1xx_until_prack() {
        let (txns, peer, peer_addr) = txn_rig().await;

        // A plain 180 arms NO resender (best-effort by design).
        txns.on_outbound(&resp(180, "z9hG4bK-i1", "1 INVITE", ""), peer_addr);
        assert!(
            recv_one(&peer, 1_200).await.is_none(),
            "a non-reliable 18x must not be proactively retransmitted"
        );

        // A reliable 183 IS retransmitted until its PRACK.
        let r183 = resp(183, "z9hG4bK-i1", "1 INVITE", "Require: 100rel\r\nRSeq: 1\r\n");
        txns.on_outbound(&r183, peer_addr);
        let got = recv_one(&peer, 2_000).await.expect("reliable 183 retransmit");
        assert!(got.starts_with("SIP/2.0 183"), "retransmit is the reliable 183: {got}");

        // The matching PRACK stops it (and is delivered to the app).
        assert!(txns.on_inbound(&prack_req("z9hG4bK-p2", "1 1 INVITE"), peer_addr));
        assert!(
            recv_one(&peer, 1_500).await.is_none(),
            "reliable-183 resender must stop on the matching PRACK"
        );
        txns.shutdown();
    }
}
