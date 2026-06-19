//! [`HealthProbe`] — OPTIONS health probing toward the B2BUA workers, riding
//! the `sip-txn` non-INVITE client transaction (port of `health/HealthProbe.ts`,
//! re-based from a hand-rolled fan-out onto the transaction layer).
//!
//! Per tick (every `interval_ms`): reap probes past their `timeout_ms` reply
//! window (each reap = one miss; `threshold` consecutive misses → `Dead`),
//! prune per-worker state for departed/recreated workers, then fan one OPTIONS
//! client transaction out to every registered worker. Replies arrive as
//! transaction events and correlate by Via branch; a 200 → `Alive`, a
//! `503 + Reason: …not-ready…` → `NotReady`, any other 503 → `Draining`. Each
//! reply's `X-Overload` payload feeds the [`WorkerLoadObserver`]. Health is
//! written through the [`WorkerRegistryControl`] seam.
//!
//! Why the transaction layer: the old fan-out sent ONE datagram per worker per
//! tick, so a single lost packet was a full miss and two losses in ~2.5 s
//! falsely flipped a healthy worker `Dead` (shedding it from selection).
//! Timer E now retransmits at T1 inside the reply window, absorbing
//! single-packet loss; correlation is the transaction branch instead of a
//! minted-Call-ID parse. The probe keeps its own reply window — Timer F is a
//! fixed 32 s, far too slow for health detection — and **cancels** the
//! transaction when the window expires, so per-probe state (here and in the
//! transaction map) does not outlive `timeout_ms` by more than a turn.
//!
//! Late-reply recovery (the 2026-05-05 k8s endurance regression): the TS source
//! kept a per-cycle `pendingByCallId[callId]` entry that its reap cleared at a
//! short fixed deadline; under sustained traffic every valid-but-late 200 OK
//! landed AFTER its entry was cleared, was silently discarded, and a worker that
//! once crossed `threshold` stayed `Dead` for the rest of the run. That class of
//! bug is structurally impossible here: a probe stays in `pending` keyed by its
//! durable transaction Via branch, and a reply correlates by that branch in
//! `handle_reply` for as long as the probe is still pending — which is governed
//! by the **tick-gated reap**, not the timeout/interval ratio. The reap runs
//! once per `interval_ms` tick and evicts only probes whose `deadline_ms <= now`
//! at that tick, so a probe lingers in `pending` until the FIRST tick at-or-after
//! its deadline (this is why a config with `timeout_ms < interval_ms` — e.g. the
//! failover harness's 10 s/1.5 s — still correlates late replies rather than
//! racing them). A reply arriving any number of ticks late therefore still
//! correlates by branch and resets the miss counter — and a reply proving
//! liveness also retires every other in-flight probe to the same worker so their
//! later reap cannot count a spurious miss. A reply that lands after its branch
//! has already been reaped, or that correlates to no `pending` probe at all (a
//! forged/replayed branch), is dropped: it can never revive a worker. Locked by
//! `tests/health_probe_late_reply.rs`.
//!
//! Identity hygiene (the dead-pod resurrection race): every pending probe
//! carries the ADDRESS it was sent to, and a reply or miss only counts while
//! the worker's *current* registry address still matches — a late 200 from a
//! dead pod's old IP cannot mark the recreated (never-probed) pod `Alive`. A
//! host move also resets the worker's miss count and overload band, so the
//! fresh pod is judged from scratch instead of inheriting the dead pod's
//! state (which excluded an idle fresh pod from selection until its first
//! `X-Overload` reply).
//!
//! Scheduling rides `tokio::time` directly (per the sip-clock ADR); the tick
//! period is `interval_ms` (the old loop slept `interval` and THEN drained for
//! `timeout`, making the effective period `interval + timeout`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, ContactSpec, GenerateOutOfDialogRequestOpts, OutOfDialogMethod, SipTransport,
    ViaSpec,
};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::{SipMessage, SipRequest};
use sip_net::UdpEndpoint;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent, TransactionLayer, TransactionLayerClosed, TxnKind};
use tokio::sync::mpsc;

use crate::addr::ProxyAddr;
use crate::load_observer::{parse_x_overload_header, WorkerLoadObserver};
use crate::registry::control::WorkerRegistryControl;
use crate::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};

/// Probe cadence (source defaults: 1 s interval, 1.5 s reply window, 2 misses
/// → dead). `interval_ms` is the tick period; `timeout_ms` is each probe's own
/// reply window (windows may overlap ticks — probes are branch-correlated).
#[derive(Debug, Clone, Copy)]
pub struct HealthProbeConfig {
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub threshold: u32,
}

impl Default for HealthProbeConfig {
    fn default() -> Self {
        Self { interval_ms: 1_000, timeout_ms: 1_500, threshold: 2 }
    }
}

/// One in-flight probe transaction, keyed by its Via branch.
struct PendingProbe {
    worker_id: String,
    /// The address this probe was sent to — replies/misses count only while
    /// the worker is still registered AT this address.
    addr: ProxyAddr,
    deadline_ms: u64,
}

/// The probe's mutable per-run state (one owner: the `run` loop).
#[derive(Default)]
struct ProbeState {
    /// branch → in-flight probe.
    pending: HashMap<String, PendingProbe>,
    /// Consecutive misses per worker id.
    misses: HashMap<String, u32>,
    /// Last address each worker was seen at — detects a host move (recreated
    /// pod) so its miss count and overload band reset.
    last_addr: HashMap<String, ProxyAddr>,
}

/// The per-probe `call_ref` stamped as the Via `cr=` custom param — the
/// transaction-layer attribution key `cancel_txns_for_call` cancels by.
fn probe_call_ref(branch: &str) -> String {
    format!("probe:{branch}")
}

/// The OPTIONS keepalive probe. Owns a UAC endpoint on the signaling fabric
/// through a dedicated [`TransactionLayer`].
pub struct HealthProbe {
    txn: TransactionLayer,
    events: mpsc::Receiver<TransactionEvent>,
    probe_host: String,
    probe_port: u16,
    registry: Arc<dyn WorkerRegistry>,
    control: Arc<dyn WorkerRegistryControl>,
    observer: Arc<WorkerLoadObserver>,
    clock: Clock,
    id_gen: Arc<IdGen>,
    config: HealthProbeConfig,
}

impl HealthProbe {
    /// Must be called within a tokio runtime (it spawns the transaction-layer
    /// owner task over `endpoint`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: Box<dyn UdpEndpoint>,
        registry: Arc<dyn WorkerRegistry>,
        control: Arc<dyn WorkerRegistryControl>,
        observer: Arc<WorkerLoadObserver>,
        clock: Clock,
        id_gen: Arc<IdGen>,
        config: HealthProbeConfig,
    ) -> Self {
        let local = endpoint.local_addr();
        let (txn, events) = TransactionLayer::spawn(
            endpoint,
            Arc::new(CustomParser::default()),
            TransactionConfig { id_gen: id_gen.clone(), ..Default::default() },
        );
        Self {
            txn,
            events,
            probe_host: local.ip().to_string(),
            probe_port: local.port(),
            registry,
            control,
            observer,
            clock,
            id_gen,
            config,
        }
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Run the probe loop forever (spawn it; abort to stop). Returns when the
    /// transaction layer closes (endpoint gone).
    pub async fn run(mut self) {
        let mut state = ProbeState::default();
        // First tick AFTER one full interval (interval_at, not interval — the
        // latter fires immediately): probing at t=0 catches workers mid-boot,
        // and their boot-time 503 would demote a pre-seeded Alive pool before
        // any traffic flowed.
        let period = Duration::from_millis(self.config.interval_ms);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if self.tick(&mut state).await.is_err() {
                        return;
                    }
                }
                ev = self.events.recv() => match ev {
                    None => return, // transaction layer gone (endpoint closed)
                    Some(TransactionEvent::Message { message, .. }) => {
                        if let SipMessage::Response(resp) = *message {
                            self.handle_reply(resp, &mut state).await;
                        }
                    }
                    // Timer F (32 s) backstop — the reap normally cancelled the
                    // transaction long before; count it only if still pending.
                    Some(TransactionEvent::Timeout { branch, .. }) => {
                        if let Some(p) = state.pending.remove(&branch) {
                            self.count_miss(p, &mut state.misses);
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }

    async fn tick(&self, state: &mut ProbeState) -> Result<(), TransactionLayerClosed> {
        let now = self.now_ms();

        // ── Reap probes past their reply window ──────────────────────────
        // Each reap = one miss; the transaction is cancelled so Timer E stops
        // retransmitting into the void for the remainder of Timer F.
        let expired: Vec<String> =
            state.pending.iter().filter(|(_, p)| p.deadline_ms <= now).map(|(b, _)| b.clone()).collect();
        for branch in expired {
            let p = state.pending.remove(&branch).expect("collected above");
            self.txn.cancel_txns_for_call(&probe_call_ref(&branch)).await?;
            self.count_miss(p, &mut state.misses);
        }

        // ── Keep per-worker state in lockstep with membership ────────────
        // Departed ordinals are dropped (bounded maps under worker churn); a
        // host move (recreated pod under the same ordinal) resets the miss
        // count and the overload band — the fresh pod is judged from scratch.
        let snapshot = self.registry.snapshot();
        for w in &snapshot {
            if state.last_addr.get(&w.id).is_some_and(|prev| prev != &w.address) {
                state.misses.remove(&w.id);
                self.observer.reset(&w.id);
            }
            state.last_addr.insert(w.id.clone(), w.address.clone());
        }
        let live = |id: &str| snapshot.iter().any(|w| w.id == id);
        state.last_addr.retain(|id, _| live(id));
        state.misses.retain(|id, _| live(id));
        self.observer.retain(live);

        // ── Fan out one OPTIONS client transaction per worker ────────────
        for w in &snapshot {
            let Some(dst) = w.address.to_socket_addr() else { continue };
            let branch = self.id_gen.new_branch();
            let req = self.build_options(w, &branch, now);
            let handle = self.txn.send_request(req, dst, TxnKind::NonInvite).await?;
            state.pending.insert(
                handle.branch().to_string(),
                PendingProbe {
                    worker_id: w.id.clone(),
                    addr: w.address.clone(),
                    deadline_ms: now + self.config.timeout_ms,
                },
            );
        }
        Ok(())
    }

    async fn handle_reply(&self, resp: SipResponse, state: &mut ProbeState) {
        let Some(branch) = resp.via.first().branch.clone() else { return };
        let Some(p) = state.pending.remove(&branch) else { return };

        // A reply proves liveness — retire any other in-flight probes to the
        // same worker so their later reap can't count a spurious miss against
        // a worker that just answered.
        let stale: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, q)| q.worker_id == p.worker_id)
            .map(|(b, _)| b.clone())
            .collect();
        for b in stale {
            state.pending.remove(&b);
            let _ = self.txn.cancel_txns_for_call(&probe_call_ref(&b)).await;
        }

        // Stale-origin guard: only the worker's CURRENT registry address may
        // report its health — a late 200 from a dead pod's old IP must not
        // resurrect the recreated, never-probed pod.
        if !self.registry.resolve(&p.worker_id).is_some_and(|w| w.address == p.addr) {
            return;
        }

        state.misses.insert(p.worker_id.clone(), 0);

        let reason = get_header(&resp.headers, "reason");
        let health = match resp.status {
            200 => WorkerHealth::Alive,
            503 => classify_503(reason),
            _ => WorkerHealth::Alive,
        };
        self.control.set_health(&p.worker_id, health);

        // Feed the AIMD observer with the self-reported overload payload. The
        // observer is decoupled from the clock (it takes `now_ms` explicitly,
        // like the TS source), so hand it the same monotonic-anchored epoch-ms
        // the probe timestamps with — under a paused runtime this advances in
        // lockstep with `tokio::time` (CLAUDE.md).
        if let Some(payload) = parse_x_overload_header(get_header(&resp.headers, "x-overload")) {
            self.observer.apply_payload(&p.worker_id, &payload, self.clock.now_ms());
        } else {
            // OPTIONS reply without a usable X-Overload header — no AIMD step,
            // just the diagnostic counter (port of HealthProbe.ts:391).
            self.observer.note_payload_missing(&p.worker_id, self.clock.now_ms());
        }
    }

    /// Count one missed reply window. Only a worker still registered at the
    /// probed address accrues it — a departed or recreated worker's silence is
    /// not the fresh pod's fault.
    fn count_miss(&self, p: PendingProbe, misses: &mut HashMap<String, u32>) {
        if !self.registry.resolve(&p.worker_id).is_some_and(|w| w.address == p.addr) {
            return;
        }
        let n = misses.entry(p.worker_id.clone()).or_insert(0);
        *n += 1;
        if *n >= self.config.threshold {
            self.control.set_health(&p.worker_id, WorkerHealth::Dead);
        }
    }

    fn build_options(&self, w: &WorkerEntry, branch: &str, now_ms: u64) -> SipRequest {
        generate_out_of_dialog_request(
            OutOfDialogMethod::Options,
            &GenerateOutOfDialogRequestOpts {
                request_uri: format!("sip:{}:{}", w.address.host, w.address.port),
                // Diagnostic only — correlation is the transaction branch.
                call_id: format!("probe-{}-{}@{}", w.id, now_ms, self.probe_host),
                from_uri: format!("sip:probe@{}", self.probe_host),
                from_tag: "probe".into(),
                to_uri: format!("sip:probe@{}:{}", w.address.host, w.address.port),
                to_tag: None,
                cseq: 1,
                via: Some(ViaSpec {
                    local_ip: self.probe_host.clone(),
                    local_port: self.probe_port,
                    transport: SipTransport::Udp,
                    branch: branch.to_string(),
                    custom_params: vec![("cr".into(), probe_call_ref(branch))],
                }),
                contact: Some(ContactSpec {
                    user: "probe".into(),
                    host: self.probe_host.clone(),
                    port: self.probe_port,
                    uri_params: vec![],
                }),
                max_forwards: Some(70),
                ..Default::default()
            },
        )
    }
}

fn classify_503(reason: Option<&str>) -> WorkerHealth {
    match reason {
        Some(r) if r.to_ascii_lowercase().contains("not-ready") => WorkerHealth::NotReady,
        _ => WorkerHealth::Draining,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_503_distinguishes_not_ready() {
        assert_eq!(classify_503(Some("SIP;cause=503;text=\"not-ready (boot drain)\"")), WorkerHealth::NotReady);
        assert_eq!(classify_503(Some("SIP;cause=503;text=\"draining\"")), WorkerHealth::Draining);
        assert_eq!(classify_503(None), WorkerHealth::Draining);
    }

    #[test]
    fn probe_call_ref_is_branch_scoped() {
        assert_eq!(probe_call_ref("z9hG4bK-1"), "probe:z9hG4bK-1");
        assert_ne!(probe_call_ref("a"), probe_call_ref("b"));
    }
}
