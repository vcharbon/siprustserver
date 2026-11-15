//! The per-call face of the mux: [`MuxNetwork`] (a per-call
//! [`SignalingNetwork`] over the shared core, built from a call's
//! [`CallRouting`](super::CallRouting)) and the [`MuxEndpoint`] of each leg
//! it binds.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use scenario_harness::claim::ClaimRule;
use scenario_harness::legpick::LegPicker;
use sip_net::queue::PacketQueue;
use sip_net::{
    BindError, BindErrorReason, BindUdpOpts, SignalingNetwork, UdpEndpoint, UndeliveredPacket,
};
use tokio::time::Instant;

use super::late_answer::SentAnswers;
use super::loss::{DropModel, TargetedDrop};
use super::retransmit::CallTxns;
use super::routing::BindDecl;
use super::rtt::RttSink;
use super::tally::CallTally;
use super::taps::{Taps, WireTap};
use super::{CallGate, CallSlot, Key, MuxCore, ReceiverEntry, Role};
use leg::MuxEndpoint;

mod leg;

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
    /// This call's caller state, shared by its caller endpoint and its slots.
    pub(super) gate: Arc<CallGate>,
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
    /// Where each bound endpoint's measured SIP round trips go; `None` = not
    /// measured.
    pub(super) rtt_sink: Option<RttSink>,
    /// The call's message tally; `None` = not tallied.
    pub(super) tally: Option<Arc<CallTally>>,
    /// The call's wire tap; `None` = not watched.
    pub(super) wire_tap: Option<WireTap>,
    /// The retransmit engine of every endpoint bound on this network.
    pub(super) engines: Mutex<Vec<Arc<CallTxns>>>,
}

impl MuxNetwork {
    /// Measure the SIP round trips of every endpoint bound on this network
    /// into `sink`.
    pub fn with_rtt_sink(mut self, sink: RttSink) -> Self {
        self.rtt_sink = Some(sink);
        self
    }

    /// Tally every datagram this network's endpoints send, re-send or have
    /// delivered into `tally`.
    pub fn with_tally(mut self, tally: Arc<CallTally>) -> Self {
        self.tally = Some(tally);
        self
    }

    /// Hand every datagram this network's endpoints send or receive to `tap`.
    pub fn with_wire_tap(mut self, tap: WireTap) -> Self {
        self.wire_tap = Some(tap);
        self
    }

    /// Whether a client transaction is still open on this call's endpoints
    /// (final not arrived, Timer B/F not expired). Never without the engine.
    pub fn client_txn_open(&self) -> bool {
        self.engines.lock().unwrap().iter().any(|txns| txns.client_open())
    }

    /// Park until every client transaction this call's endpoints have open
    /// has ended (final received, or Timer B/F), retransmitting meanwhile:
    /// what a failed call waits for before its endpoints drop, so its
    /// teardown CANCEL or BYE survives a loss, and what a call whose in-dialog
    /// request awaits its final waits for. Immediate without the engine.
    pub async fn client_txns_ended(&self) {
        let engines: Vec<Arc<CallTxns>> = self.engines.lock().unwrap().clone();
        for txns in engines {
            txns.clients_ended().await;
        }
    }

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
                    message: format!("callee endpoint {} bound without a declared leg", opts.addr),
                })
            }
            None => None,
        };
        // The endpoint's role follows the RESOLVED bind, not the socket: on a
        // shared-vantage socket the caller endpoint and pending UAS receivers
        // coexist on one addr.
        let role = if uas.is_some() { Role::Callee } else { Role::Caller };
        if role == Role::Caller {
            self.gate.has_caller.store(true, Ordering::Relaxed);
        }
        // One loss model + (optional) retransmit engine per endpoint, shared
        // between this endpoint (outbound) and the registry entry the inbound
        // `route` path consults, so both directions and the resend tasks agree.
        // A leg-scoped targeted drop arms only on its named leg's endpoint.
        let drop_nth = self.drop_nth.filter(|t| match t.leg {
            None => true,
            Some(leg) => uas.as_ref().map(|(l, _)| l.as_str()) == Some(leg),
        });
        let drop = Arc::new(DropModel::new(self.drop_rate, self.next_drop_seed(), drop_nth));
        let leg = uas.as_ref().map(|(label, _)| label.as_str());
        let taps = Taps::new(self.rtt_sink.clone(), self.tally.clone(), self.wire_tap.clone(), leg);
        let txns = self.retransmit.then(|| {
            Arc::new(CallTxns::new(
                mux.endpoint.clone(),
                drop.clone(),
                mux.stats.clone(),
                taps.clone(),
            ))
        });
        if let Some(txns) = &txns {
            self.engines.lock().unwrap().push(txns.clone());
        }
        let answers = Arc::new(SentAnswers::default());

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
            // A key cooling after a failed call is refused in the same step
            // (under the registry lock) that would claim it.
            if !g.by_token.contains_key(&token) && self.core.key_cooling(&token) {
                return Err(BindError {
                    reason: BindErrorReason::HeldBack,
                    addr: opts.addr,
                    message: format!("correlation key {token:?} is cooling after a failed call"),
                });
            }
            let pending_ttl = self.core.pending_ttl;
            let slot = match g.by_token.entry(token.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let slot = e.into_mut();
                    if slot.owner != self.owner {
                        // Claims are scoped (token, rule) per call instance —
                        // a colliding token (a number shared under To-user or
                        // From-user correlation) fails HERE, before any leg
                        // could be misdelivered.
                        mux.stats.token_collision.fetch_add(1, Ordering::Relaxed);
                        return Err(BindError {
                            reason: BindErrorReason::AlreadyBound,
                            addr: opts.addr,
                            message: format!(
                                "correlation token {token:?} already registered by a \
                                 concurrent call (a number shared under To-user or \
                                 From-user correlation?)"
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
                    gate: self.gate.clone(),
                }),
            };
            keyset.lock().unwrap().push(Key::Token { token, label: label.clone() });
            slot.receivers.push(ReceiverEntry {
                label,
                queue: queue.clone(),
                keyset: keyset.clone(),
                drop: drop.clone(),
                txns: txns.clone(),
                taps: taps.clone(),
                answers: answers.clone(),
                claim,
                claimed: false,
            });
        }

        // Under from-user correlation the call's key IS its caller's From
        // user, so a caller endpoint holds the key its INVITE must present.
        let caller_key =
            (role == Role::Caller && mux.correlation.is_from_user()).then(|| self.token.clone());
        Ok(Box::new(MuxEndpoint {
            local: mux.addr,
            role,
            caller_key,
            mux: mux.clone(),
            queue,
            keyset,
            gate: self.gate.clone(),
            invite_sent: AtomicBool::new(false),
            queue_max: mux.queue_max,
            drop,
            txns,
            taps,
            invites: (role == Role::Caller).then(Default::default),
            answers,
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

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
