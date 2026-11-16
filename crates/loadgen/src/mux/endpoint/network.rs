//! A call's [`MuxNetwork`]: its construction over the shared [`MuxCore`],
//! what watches its endpoints (round trips, the tally, the wire and loss
//! taps) and its endpoints' open transactions.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::super::retransmit::CallTxns;
use super::super::rtt::RttSink;
use super::super::tally::CallTally;
use super::super::taps::{LossTap, WireTap};
use super::super::{routing, CallGate, CallRouting, MuxCore, TargetedDrop};
use super::MuxNetwork;

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

    /// Hand every datagram the loss model drops on this network's endpoints
    /// to `tap`.
    pub fn with_loss_tap(mut self, tap: LossTap) -> Self {
        self.loss_tap = Some(tap);
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
    pub(super) fn next_drop_seed(&self) -> u64 {
        self.drop_seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
    }
}

impl MuxCore {
    /// Build a per-call [`SignalingNetwork`] view from a [`CallRouting`] (the
    /// single call token + each callee leg's label + any same-socket picker).
    /// No simulated packet loss and no auto-retransmit.
    pub fn network(self: &Arc<Self>, routing: CallRouting) -> MuxNetwork {
        self.network_tuned(routing, 0.0, false, 0, None)
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
    /// `drop_nth` optionally adds the DETERMINISTIC targeted drop
    /// ([`TargetedDrop`]) on top of the probabilistic rate.
    pub fn network_tuned(
        self: &Arc<Self>,
        routing: CallRouting,
        drop_rate: f64,
        retransmit: bool,
        seed: u64,
        drop_nth: Option<TargetedDrop>,
    ) -> MuxNetwork {
        let dispense = dispense(routing.binds);
        MuxNetwork {
            core: self.clone(),
            owner: self.call_seq.fetch_add(1, Ordering::Relaxed),
            token: routing.token,
            dispense,
            pickers: routing.pickers,
            cursor: Mutex::new(HashMap::new()),
            gate: Arc::new(CallGate::default()),
            drop_rate,
            retransmit,
            drop_seed: AtomicU64::new(seed | 1),
            drop_nth,
            rtt_sink: None,
            tally: None,
            wire_tap: None,
            loss_tap: None,
            engines: Mutex::new(Vec::new()),
        }
    }
}

/// The per-addr bind-order dispense queue: each `bind_udp(addr)` takes the
/// next declared entry for that addr, so several receivers (and, on a
/// shared-vantage socket, the caller too) share a socket; the driver binds
/// the agents in declaration order.
fn dispense(
    binds: Vec<(SocketAddr, routing::BindDecl)>,
) -> HashMap<SocketAddr, Vec<routing::BindDecl>> {
    let mut dispense: HashMap<SocketAddr, Vec<routing::BindDecl>> = HashMap::new();
    for (addr, decl) in binds {
        dispense.entry(addr).or_default().push(decl);
    }
    dispense
}
