//! The per-call face of the mux: [`CallRouting`] (what a scenario declares),
//! [`MuxNetwork`] (a per-call [`SignalingNetwork`] over the shared core) and
//! [`MuxEndpoint`] (one leg's endpoint: inbox + shared send socket).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use scenario_harness::claim::ClaimRule;
use scenario_harness::legpick::LegPicker;
use sip_message::sniff::call_id;
use sip_net::queue::PacketQueue;
use sip_net::{
    BindError, BindErrorReason, BindUdpOpts, SendError, SendTap, SignalingNetwork, UdpEndpoint,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};
use tokio::time::Instant;

use super::loss::{DropDir, DropModel, TargetedDrop};
use super::retransmit::CallTxns;
use super::{CallSlot, Delivery, Key, MuxCore, MuxSocket, ReceiverEntry, Role};

/// One declared bind on a mux endpoint, dispensed in declaration order as the
/// call's agents call `bind_udp(addr)`.
#[derive(Clone)]
pub(super) enum BindDecl {
    /// A caller (UAC) endpoint — registers its dialog from its first outbound
    /// request; holds no token-slot receiver. Declared explicitly only on a
    /// SHARED-vantage socket (caller + pending UAS receivers on one addr).
    Caller,
    /// A pending UAS receiver: `label` = the agent name; `claim` = the
    /// scenario-declared rule that assigns its inbound initial INVITE
    /// (`None` = legacy count/picker resolution).
    Uas { label: String, claim: Option<ClaimRule> },
}

/// The per-call routing a scenario declares before its agents bind: the single
/// correlation token every leg of the call carries, the binds in **bind
/// order** per addr (caller and/or UAS receivers — several on one `addr` form
/// a shared socket), and how a shared socket assigns each arriving leg: a
/// compiled [`LegPicker`], or per-leg data [`ClaimRule`]s (a scenario built
/// from capture data at case start) — never both on one socket. The mux never
/// reads `X-Api-Call` or any URI to route legs — that is the scenario's job,
/// expressed here and in how it dials the SUT.
#[derive(Clone, Default)]
pub struct CallRouting {
    pub(super) token: String,
    pub(super) binds: Vec<(SocketAddr, BindDecl)>,
    pub(super) pickers: HashMap<SocketAddr, LegPicker>,
}

impl CallRouting {
    /// Start a routing for a call with correlation token `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), binds: Vec::new(), pickers: HashMap::new() }
    }
    /// Declare a callee leg: an agent labelled `label` binds on `addr`. Declare
    /// legs in the order the driver binds the agents (several on one `addr` form
    /// a shared socket).
    pub fn leg(mut self, addr: SocketAddr, label: impl Into<String>) -> Self {
        self.binds.push((addr, BindDecl::Uas { label: label.into(), claim: None }));
        self
    }
    /// Declare a pending-UAS claim: the agent labelled `label` binds on `addr`
    /// and owns the inbound initial INVITE `rule` selects within this call's
    /// token scope. Consumed once; an INVITE matching no pending claim is
    /// counted `unclaimed` (never misdelivered). Same bind-order contract as
    /// [`leg`](Self::leg); claims and a [`picker`](Self::picker) are mutually
    /// exclusive per socket, as are claim and non-claim legs.
    pub fn claim(mut self, addr: SocketAddr, label: impl Into<String>, rule: ClaimRule) -> Self {
        self.binds.push((addr, BindDecl::Uas { label: label.into(), claim: Some(rule) }));
        self
    }
    /// Declare a caller (UAC) bind on `addr` — needed only when the caller
    /// SHARES a socket with pending UAS receivers (the pcap mux vantage, where
    /// the SUT dials new legs back to the ip:port that originated leg A). On
    /// an addr with any declared bind, EVERY bind must be declared, in bind
    /// order; an addr with none keeps its endpoint-role default.
    pub fn caller(mut self, addr: SocketAddr) -> Self {
        self.binds.push((addr, BindDecl::Caller));
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
    pub(super) core: Arc<MuxCore>,
    /// This call instance's id — the scope of its token slot. A second call
    /// binding the same token fails explicitly (`token_collision`).
    pub(super) owner: u64,
    pub(super) token: String,
    /// Bind-order declared binds per addr (dispensed by `cursor`).
    pub(super) dispense: HashMap<SocketAddr, Vec<BindDecl>>,
    pub(super) pickers: HashMap<SocketAddr, LegPicker>,
    pub(super) cursor: Mutex<HashMap<SocketAddr, usize>>,
    /// Per-call simulated packet-loss rate applied to every endpoint bound on
    /// this network (0 = off). Each endpoint gets its own RNG seeded off
    /// `drop_seed` so alice/bob/charlie drop independently.
    pub(super) drop_rate: f64,
    /// Whether each endpoint runs the per-call SIP retransmit engine.
    pub(super) retransmit: bool,
    pub(super) drop_seed: AtomicU64,
    /// Optional deterministic targeted drop, applied per endpoint (each bound
    /// endpoint tracks its own matching-request arrivals).
    pub(super) drop_nth: Option<TargetedDrop>,
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
        // Resolve WHAT this bind is FIRST (bind order over this addr's declared
        // binds) — the leg-scoped targeted drop below keys off the label. An
        // addr with no declared binds keeps its endpoint-role default: a caller
        // endpoint, or (callee) an undeclared-leg error.
        let decl = match self.dispense.get(&opts.addr) {
            Some(list) => {
                let mut cur = self.cursor.lock().unwrap();
                let n = cur.entry(opts.addr).or_insert(0);
                let idx = *n;
                *n += 1;
                Some(list.get(idx).cloned().ok_or_else(|| BindError {
                    reason: BindErrorReason::OsError,
                    addr: opts.addr,
                    message: format!(
                        "endpoint {} bound more times than declared (#{idx})",
                        opts.addr
                    ),
                })?)
            }
            None => None,
        };
        let uas: Option<(String, Option<ClaimRule>)> = match decl {
            Some(BindDecl::Caller) => None,
            Some(BindDecl::Uas { label, claim }) => Some((label, claim)),
            None if mux.role == Role::Callee => {
                return Err(BindError {
                    reason: BindErrorReason::OsError,
                    addr: opts.addr,
                    message: format!(
                        "callee endpoint {} bound without a declared leg",
                        opts.addr
                    ),
                })
            }
            None => None,
        };
        // The endpoint's role follows the RESOLVED bind, not the socket: on a
        // shared-vantage socket the caller endpoint and pending UAS receivers
        // coexist on one addr.
        let role = if uas.is_some() { Role::Callee } else { Role::Caller };
        // One loss model + (optional) retransmit engine per endpoint, shared
        // between this endpoint (outbound) and the registry entry the inbound
        // `route` path consults, so both directions and the resend tasks agree.
        // A leg-scoped targeted drop arms only on its named leg's endpoint.
        let drop_nth = self.drop_nth.filter(|t| match t.leg {
            None => true,
            Some(leg) => uas.as_ref().map(|(l, _)| l.as_str()) == Some(leg),
        });
        let drop = Arc::new(DropModel::new(self.drop_rate, self.next_drop_seed(), drop_nth));
        let txns = self
            .retransmit
            .then(|| Arc::new(CallTxns::new(mux.endpoint.clone(), drop.clone(), mux.stats.clone())));

        if let Some((label, claim)) = uas {
            let claim_mode = claim.is_some();
            if claim_mode && self.pickers.contains_key(&opts.addr) {
                return Err(BindError {
                    reason: BindErrorReason::OsError,
                    addr: opts.addr,
                    message: format!(
                        "endpoint {}: declare claims OR a picker on a shared socket, not both",
                        opts.addr
                    ),
                });
            }
            let token = self.token.clone();
            let mut g = mux.reg.lock().unwrap();
            let pending_ttl = self.core.pending_ttl;
            let slot = match g.by_token.entry(token.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let slot = e.into_mut();
                    if slot.owner != self.owner {
                        // Claims are scoped (token, rule) per call instance —
                        // a colliding token (To-user correlation + a shared
                        // callee number) fails HERE, before any leg could be
                        // misdelivered.
                        mux.stats.token_collision.fetch_add(1, Ordering::Relaxed);
                        return Err(BindError {
                            reason: BindErrorReason::AlreadyBound,
                            addr: opts.addr,
                            message: format!(
                                "correlation token {token:?} already registered by a \
                                 concurrent call (shared callee number under To-user \
                                 correlation?)"
                            ),
                        });
                    }
                    if slot.claim_mode != claim_mode {
                        return Err(BindError {
                            reason: BindErrorReason::OsError,
                            addr: opts.addr,
                            message: format!(
                                "endpoint {}: mixed claim and non-claim receivers on one \
                                 token slot",
                                opts.addr
                            ),
                        });
                    }
                    slot
                }
                std::collections::hash_map::Entry::Vacant(v) => v.insert(CallSlot {
                    receivers: Vec::new(),
                    picker: self.pickers.get(&opts.addr).cloned(),
                    owner: self.owner,
                    claim_mode,
                    order_fired: 0,
                    arrived: false,
                    // The leg should arrive within the call's recv window; a generous
                    // multiple guards a slow SUT while still reaping a no-show.
                    // INVARIANT: this deadline (`pending_ttl * 4`) must exceed one
                    // callee `recv_timeout`, or the reaper could close a LIVE (but
                    // not-yet-`arrived`) receiver's queue out from under a pending
                    // `try_receive`. The harness sets `pending_ttl == recv_timeout`,
                    // so the 4× margin holds; keep `pending_ttl >= recv_timeout`.
                    deadline: Instant::now() + pending_ttl * 4,
                }),
            };
            keyset.lock().unwrap().push(Key::Token { token, label: label.clone() });
            slot.receivers.push(ReceiverEntry {
                label,
                queue: queue.clone(),
                keyset: keyset.clone(),
                drop: drop.clone(),
                txns: txns.clone(),
                claim,
                claimed: false,
            });
        }

        Ok(Box::new(MuxEndpoint {
            local: mux.addr,
            role,
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
    /// here in `send_to`; inbound loss is applied in the demux route path (above
    /// the retransmit engine, so a lost inbound datagram is truly gone and the
    /// peer's retransmit re-delivers it fresh).
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
        // recovers the call. The targeted OUTBOUND drop fires here too (a
        // deterministic single-shot the engine's Timer-E resend then heals).
        if self.drop.drops() || self.drop.targeted_hit(buf, DropDir::Outbound) {
            self.mux.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.mux.endpoint.send_to(buf, dst).await
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

    fn install_recv_tap(&self, tap: sip_net::RecvTap) -> bool {
        self.queue.install_tap(tap);
        true
    }

    fn install_send_tap(&self, tap: SendTap) -> bool {
        // Re-emissions come from the per-call retransmit engine; with no engine
        // (`--auto-retransmit` off) there is nothing to tap. The engine handle is
        // shared with the inbound `route` path, so one install covers both the
        // proactive (Timer A/E/G) and reactive (re-ACK / re-answer) resends.
        match &self.txns {
            Some(txns) => {
                txns.set_sendtap(tap);
                true
            }
            None => false,
        }
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
                // slot once its last receiver leaves. A claim that never fired is
                // counted on its way out (the scenario's expected leg never came).
                Key::Token { token, label } => {
                    if let Some(slot) = g.by_token.get_mut(&token) {
                        if let Some(pos) = slot.receivers.iter().position(|r| r.label == label) {
                            let r = slot.receivers.remove(pos);
                            if r.claim.is_some() && !r.claimed {
                                self.mux.stats.claim_unfired.fetch_add(1, Ordering::Relaxed);
                            }
                        }
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
