//! [`HealthProbe`]: OPTIONS health probing of the workers, each probe a
//! `sip-txn` non-INVITE client transaction.
//!
//! The first round leaves as soon as the registry holds its first listing and
//! latches the proxy's [`FirstView`] (ADR-0050 §11); then one round leaves every
//! `interval_ms`. A round drops the state of departed workers, resets a moved
//! worker's, and sends one OPTIONS to every worker the registry holds for
//! judging ([`WorkerRegistry::probed`]), ready or not. Each probe is reaped at
//! the end of its own `timeout_ms` reply window: one miss, its transaction
//! cancelled. `threshold` consecutive misses make the worker `Dead`, so the
//! verdict lands [`HealthProbeConfig::verdict_after_first_miss`] after the first
//! unanswered probe left.
//!
//! A reply correlates by its Via branch to a probe still in its window, however
//! many ticks the window spans. A reaped probe stays awaited until its slow
//! window (`slow_window_ms` from its send) ends: an answer then is late
//! (ADR-0069). It stamps the proxy's [`WorkerReach`], and the worker's late
//! stamp when it would read the worker `Alive` and names no other
//! `X-Incarnation` than the last one read; it does nothing else: it never
//! clears a miss, revives a worker or retires a probe. A reply naming no
//! awaited probe is dropped. Every final inside its window stamps the
//! [`WorkerReach`] and retires the worker's other awaited probes, whose reap
//! would count a spurious miss. From the worker's current address it also
//! clears the miss count, feeds its `X-Overload` payload to the
//! [`WorkerLoadObserver`], and is read on the two axes of ADR-0050 §3
//! ([`read_answer`]): a `200` makes the worker `Alive`, sets its acceptance from
//! `X-Readiness` (absent reads `accept`) and writes the workers' advertised
//! capabilities; a `503` sets acceptance to `refuse` and leaves health as it
//! was; any other final makes it `Alive` and keeps acceptance. A `Slow` worker
//! reads `Alive` only at its `recover_probes`-th answer in time in a row.
//! Health and acceptance are written through the [`WorkerRegistryControl`]
//! seam.
//!
//! The `threshold`-th consecutive miss reads the worker `Slow` while it
//! answered late within the slow window, `Dead` otherwise; a `Dead` worker
//! stays so. A worker turning `Alive` from `Dead` or `Slow` restarts its admit
//! cap low ([`WorkerLoadObserver::restart_cap`]). A write that ends a spell in
//! which no worker served tells the [`WorkerReach`] whether the worker it reads
//! serving is the process last read serving before the spell
//! ([`WorkerReach::rejoined`]).
//!
//! A reply or a miss counts only while the worker's current registry address is
//! the one the probe went to: a late answer to a recreated worker's old address
//! never marks the new one `Alive`. A host move resets the worker's miss count
//! and overload band. An answer naming another `X-Incarnation` than the last one
//! read is a new process at the same address: the worker is reset to `Unknown`
//! and its miss count and overload band cleared before the answer applies
//! (ADR-0050 §8).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use call::TxnKind;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::worker_report;
use sip_message::SipMessage;
use sip_net::UdpEndpoint;
use sip_txn::{
    IdGen, TransactionConfig, TransactionEvent, TransactionLayer, TransactionLayerClosed,
};
use tokio::time::Instant;

use super::capabilities::WorkerCapabilities;
use super::first_view::{FirstRound, FirstView};
use super::reach::WorkerReach;
use super::stated_host::stated_host;
use crate::addr::ProxyAddr;
use crate::load_observer::{parse_x_overload_header, WorkerLoadObserver, X_OVERLOAD};
use crate::observability::ProxyMetrics;
use crate::registry::control::WorkerRegistryControl;
use crate::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use slow::SlowLedger;

mod answer;
mod config;
mod options;
mod slow;

pub use answer::{read_answer, AnswerReading};
pub use config::HealthProbeConfig;

/// How often a probe waiting for the registry's first listing looks again.
const LISTING_POLL: Duration = Duration::from_millis(20);

/// One awaited probe transaction, keyed by its Via branch.
#[derive(Clone)]
struct PendingProbe {
    worker_id: String,
    /// The address this probe was sent to — replies/misses count only while
    /// the worker is still registered AT this address.
    addr: ProxyAddr,
    /// The last instant of its reply window: unanswered by then, one miss.
    window_ends: Instant,
    /// The last instant of its slow window: an answer by then is late.
    late_until: Instant,
    /// Its reply window ended unanswered: counted a miss, awaited late.
    missed: bool,
}

impl PendingProbe {
    /// When this probe is next reaped: at its reply window's end, then at its
    /// slow window's.
    fn next_reap(&self) -> Instant {
        if self.missed {
            self.late_until
        } else {
            self.window_ends
        }
    }
}

/// The probe's mutable per-run state (one owner: the `run` loop).
#[derive(Default)]
struct ProbeState {
    pending: HashMap<String, PendingProbe>,
    /// Consecutive misses per worker id.
    misses: HashMap<String, u32>,
    /// Last address each worker was seen at — detects a host move (recreated
    /// pod) so its miss count and overload band reset.
    last_addr: HashMap<String, ProxyAddr>,
    /// The `X-Incarnation` each worker last answered with — detects a restart
    /// at the same address.
    incarnation: HashMap<String, u64>,
    /// The `X-Incarnation` each worker had when last read serving.
    served_as: HashMap<String, u64>,
    /// Each worker's lateness (ADR-0069).
    ledger: SlowLedger,
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
    events: sip_txn::TransactionEvents,
    probe_host: String,
    probe_port: u16,
    registry: Arc<dyn WorkerRegistry>,
    control: Arc<dyn WorkerRegistryControl>,
    observer: Arc<WorkerLoadObserver>,
    clock: Clock,
    id_gen: Arc<IdGen>,
    config: HealthProbeConfig,
    /// The sink of probe misses, each a `response_timeout` of an internal peer
    /// at the probed worker's address; `None` records nothing.
    metrics: Option<Arc<ProxyMetrics>>,
    /// Latched once the first round over the first listing has completed.
    first_view: Arc<FirstView>,
    /// Stamped by every answer a probe correlates.
    reach: Arc<WorkerReach>,
    /// What every `200` inside its reply window advertises.
    capabilities: Arc<WorkerCapabilities>,
}

impl HealthProbe {
    /// Must be called within a tokio runtime (it spawns the transaction-layer
    /// owner task over `endpoint`). `parser` is the process's ingress parser:
    /// the workers' answers are read as the rest of the wire is.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: Box<dyn UdpEndpoint>,
        parser: CustomParser,
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
            Arc::new(parser),
            TransactionConfig::new(id_gen.clone()),
        );
        Self {
            txn,
            events,
            probe_host: stated_host(local),
            probe_port: local.port(),
            registry,
            control,
            observer,
            clock,
            id_gen,
            config,
            metrics: None,
            first_view: FirstView::pending(),
            reach: WorkerReach::unheard(),
            capabilities: WorkerCapabilities::unheard(),
        }
    }

    /// The proxy's first worker view, which this probe latches; the routing
    /// core and the readiness routes read the same one. Without it the probe
    /// latches a view of its own nobody reads.
    pub fn with_first_view(mut self, view: Arc<FirstView>) -> Self {
        self.first_view = view;
        self
    }

    /// The proxy's reach, which every answer this probe correlates stamps;
    /// the runner's fitness reads the same one. Without it the probe stamps a
    /// reach of its own nobody reads.
    pub fn with_reach(mut self, reach: Arc<WorkerReach>) -> Self {
        self.reach = reach;
        self
    }

    /// The workers' last advertisement, which every `200` inside its reply
    /// window writes; the routing core's supervision answer reads the same
    /// one. Without it the probe writes one of its own nobody reads.
    pub fn with_capabilities(mut self, capabilities: Arc<WorkerCapabilities>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Records each probe miss as
    /// `sip_proxy_peer_failures_total{scope="internal",kind="response_timeout"}`;
    /// without it misses are not recorded.
    pub fn with_metrics(mut self, metrics: Arc<ProxyMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    /// Run the probe loop forever (spawn it; abort to stop). Returns when the
    /// transaction layer closes (endpoint gone).
    ///
    /// The first round leaves as soon as the registry holds its first
    /// listing, then one every `interval_ms`. The first view is latched once
    /// every probe of the first round has left the in-flight set or the
    /// round's reply window (`timeout_ms`) has run out, after the answers'
    /// health is written.
    ///
    /// An answer queued by the end of its reply window is read before that
    /// window is reaped, and is no miss.
    pub async fn run(mut self) {
        let mut state = ProbeState::default();
        while !self.registry.synced() {
            tokio::time::sleep(LISTING_POLL).await;
        }
        if self.tick(&mut state).await.is_err() {
            return;
        }
        let mut first_round = Some(FirstRound::of(state.pending.keys().cloned()));
        let round_window = tokio::time::sleep(Duration::from_millis(self.config.timeout_ms));
        tokio::pin!(round_window);
        let period = self.config.period();
        #[expect(clippy::disallowed_methods, reason = "the period is at least 1 ms")]
        let mut tick = tokio::time::interval_at(sip_clock::deadline::from_now(period), period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let in_window = |b: &str| state.pending.get(b).is_some_and(|p| !p.missed);
            if first_round.as_ref().is_some_and(|r| r.settled(in_window)) {
                self.first_view.latch();
                first_round = None;
            }
            let reap_at = state.pending.values().map(PendingProbe::next_reap).min();
            tokio::select! {
                biased;
                ev = self.events.recv() => match ev {
                    None => return,
                    Some(TransactionEvent::Message { message, .. }) => {
                        if let SipMessage::Response(resp) = *message {
                            self.handle_reply(resp, &mut state).await;
                        }
                    }
                    Some(TransactionEvent::Timeout { branch, .. }) => {
                        if let Some(p) = state.pending.remove(branch.as_str()) {
                            if !p.missed {
                                self.count_miss(&p, &mut state);
                            }
                        }
                    }
                    Some(_) => {}
                },
                () = sleep_until_or_never(reap_at.map(config::past_the_window)) => {
                    if self.reap(&mut state).await.is_err() {
                        return;
                    }
                }
                _ = tick.tick() => {
                    if self.tick(&mut state).await.is_err() {
                        return;
                    }
                }
                _ = &mut round_window, if first_round.is_some() => {
                    self.first_view.latch();
                    first_round = None;
                }
            }
        }
    }

    /// Reap every probe whose reply window has ended, each one miss, still
    /// awaited late; then every probe whose slow window has ended, its
    /// transaction cancelled so Timer E stops retransmitting into the void for
    /// the remainder of Timer F.
    async fn reap(&self, state: &mut ProbeState) -> Result<(), TransactionLayerClosed> {
        let now = Instant::now();
        let missed: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, p)| !p.missed && p.window_ends < now)
            .map(|(b, _)| b.clone())
            .collect();
        for branch in missed {
            let Some(p) = state.pending.get_mut(branch.as_str()) else { continue };
            p.missed = true;
            let p = p.clone();
            self.count_miss(&p, state);
        }
        let ended: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, p)| p.missed && p.late_until < now)
            .map(|(b, _)| b.clone())
            .collect();
        for branch in ended {
            state.pending.remove(branch.as_str());
            self.txn.cancel_txns_for_call(&probe_call_ref(&branch)).await?;
        }
        Ok(())
    }

    /// One round: drops the state of every worker the registry no longer
    /// probes, resets the miss count, incarnations, lateness and overload band
    /// of a worker at a new address, reads every worker's `Slow` spell, and
    /// sends one OPTIONS client transaction to each probed worker.
    async fn tick(&self, state: &mut ProbeState) -> Result<(), TransactionLayerClosed> {
        let now = self.now_ms();
        let window = Duration::from_millis(self.config.timeout_ms);
        let slow_window = self.config.slow_window();

        let snapshot = self.registry.probed();
        for w in &snapshot {
            if state.last_addr.get(&w.id).is_some_and(|prev| prev != &w.address) {
                state.misses.remove(&w.id);
                state.incarnation.remove(&w.id);
                state.served_as.remove(&w.id);
                state.ledger.forget(&w.id);
                self.observer.reset(&w.id);
            }
            state.last_addr.insert(w.id.clone(), w.address.clone());
        }
        let live = |id: &str| snapshot.iter().any(|w| w.id == id);
        state.last_addr.retain(|id, _| live(id));
        state.misses.retain(|id, _| live(id));
        state.incarnation.retain(|id, _| live(id));
        state.served_as.retain(|id, _| live(id));
        state.ledger.retain(live);
        self.observer.retain(live);
        for w in &snapshot {
            self.read_spell(&w.id, w.health, state);
        }

        for w in &snapshot {
            let Some(dst) = w.address.to_socket_addr() else { continue };
            let branch = self.id_gen.new_branch();
            let Some(req) = self.build_options(w, &branch, now) else { continue };
            let handle = self.txn.send_request(req, dst, TxnKind::NonInvite).await?;
            state.pending.insert(
                handle.branch().to_string(),
                PendingProbe {
                    worker_id: w.id.clone(),
                    addr: w.address.clone(),
                    window_ends: sip_clock::deadline::from_now(window),
                    late_until: sip_clock::deadline::from_now(slow_window),
                    missed: false,
                },
            );
        }
        Ok(())
    }

    /// Applies a final that correlates to an awaited probe: a late one stamps
    /// the worker only ([`answered_late`](Self::answered_late)). Its load
    /// payload lands before the health it comes with is published, so no sweep
    /// reads a worker alive on an old payload.
    ///
    /// Health and acceptance are published in one write: a restarted worker
    /// that answers `200` reads `Alive` at once, never `Unknown` in between.
    async fn handle_reply(&self, resp: SipResponse, state: &mut ProbeState) {
        if resp.status() < 200 {
            return;
        }
        let Some(branch) = resp.via().first().branch().map(str::to_owned) else { return };
        let Some(p) = state.pending.remove(branch.as_str()) else { return };
        self.reach.answered();
        if p.missed {
            self.answered_late(&p, &resp, state);
            return;
        }

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

        if !self.still_at(&p) {
            return;
        }

        let mut restarted = false;
        if let Some(gen) = worker_report::incarnation(&resp) {
            let previous = state.incarnation.insert(p.worker_id.clone(), gen);
            if previous.is_some_and(|prev| prev != gen) {
                tracing::info!(
                    node = observe::node(),
                    worker = %p.worker_id,
                    from = previous,
                    to = gen,
                    "worker incarnation changed"
                );
                self.observer.reset(&p.worker_id);
                restarted = true;
            }
        }
        state.misses.insert(p.worker_id.clone(), 0);
        let in_time = state.ledger.answered_in_time(&p.worker_id);
        if resp.status() == 200 {
            self.capabilities.advertised(resp.headers());
        }

        if let Some(payload) = parse_x_overload_header(resp.raw(X_OVERLOAD).next()) {
            self.observer.apply_payload(&p.worker_id, &payload, self.clock.now_ms());
        } else {
            self.observer.note_payload_missing(&p.worker_id, self.clock.now_ms());
        }

        let readiness = worker_report::readiness(&resp);
        let reading = read_answer(resp.status(), readiness.as_ref());
        let before = self.health_of(&p.worker_id);
        let recovering = before == Some(WorkerHealth::Slow) && !restarted;
        let health = match reading.health {
            Some(WorkerHealth::Alive) if recovering && in_time < self.config.recover_probes => None,
            health => health.or(restarted.then_some(WorkerHealth::Unknown)),
        };
        let blind = self.blind();
        self.control.set_view(&p.worker_id, health, reading.acceptance);
        let after = self.health_of(&p.worker_id);
        if after == Some(WorkerHealth::Alive)
            && matches!(before, Some(WorkerHealth::Dead | WorkerHealth::Slow))
        {
            self.observer.restart_cap(&p.worker_id, self.clock.now_ms());
        }
        self.wrote(&p.worker_id, after, blind, state);
    }

    /// A probe of `p`'s worker was answered past its reply window, inside its
    /// slow window: the worker's late stamp, here and in the registry, when
    /// the answer would read it `Alive` and names no other incarnation than
    /// the last one read. A late `503`, or a new process's late answer, says
    /// nothing of whether the worker serves its dialogs.
    fn answered_late(&self, p: &PendingProbe, resp: &SipResponse, state: &mut ProbeState) {
        if !self.still_at(p) {
            return;
        }
        let reading = read_answer(resp.status(), worker_report::readiness(resp).as_ref());
        let last_read = state.incarnation.get(&p.worker_id).copied();
        let other_process = worker_report::incarnation(resp)
            .zip(last_read)
            .is_some_and(|(stated, last)| stated != last);
        if reading.health != Some(WorkerHealth::Alive) || other_process {
            return;
        }
        state.ledger.answered_late(&p.worker_id, Instant::now());
        self.control.note_late_answer(&p.worker_id, self.config.slow_window());
        if let Some(metrics) = &self.metrics {
            metrics.record_probe_late_answer(&p.worker_id);
        }
    }

    /// The health the registry reads `id` at, judged or not.
    fn health_of(&self, id: &str) -> Option<WorkerHealth> {
        self.registry.probed().into_iter().find(|w| w.id == id).map(|w| w.health)
    }

    /// Whether the view reads no worker serving its dialogs.
    fn blind(&self) -> bool {
        !self.registry.snapshot().iter().any(WorkerEntry::serves_dialogs)
    }

    /// After a health write that left `id` reading `after`, on a view `blind`
    /// before it: reads its `Slow` spell, and, when it serves, the incarnation
    /// it serves as. A write that ends the blind view tells the reach whether
    /// `id` is the process last read serving.
    fn wrote(&self, id: &str, after: Option<WorkerHealth>, blind: bool, state: &mut ProbeState) {
        let Some(health) = after else { return };
        self.read_spell(id, health, state);
        if !matches!(health, WorkerHealth::Alive | WorkerHealth::Slow) {
            return;
        }
        let current = state.incarnation.get(id).copied();
        if blind && !self.blind() {
            let known = current.is_some() && state.served_as.get(id).copied() == current;
            self.reach.rejoined(known);
        }
        if let Some(gen) = current {
            state.served_as.insert(id.to_string(), gen);
        }
    }

    /// Read `id`'s health into its `Slow` spell: a spell that begins is
    /// counted.
    fn read_spell(&self, id: &str, health: WorkerHealth, state: &mut ProbeState) {
        if state.ledger.read(id, health) {
            if let Some(metrics) = &self.metrics {
                metrics.record_worker_slow(id);
            }
        }
    }

    /// Whether `p`'s worker is still judged at the address it was probed at.
    fn still_at(&self, p: &PendingProbe) -> bool {
        self.registry.probed().iter().any(|w| w.id == p.worker_id && w.address == p.addr)
    }

    /// Count one missed reply window. Only a worker still registered at the
    /// probed address accrues it — a departed or recreated worker's silence is
    /// not the fresh pod's fault. The `threshold`-th in a row reads the worker
    /// `Slow` while it answered late within the slow window, else `Dead`; a
    /// `Dead` worker stays `Dead`.
    fn count_miss(&self, p: &PendingProbe, state: &mut ProbeState) {
        if !self.still_at(p) {
            return;
        }
        if let Some(metrics) = &self.metrics {
            if let Some(dst) = p.addr.to_socket_addr() {
                metrics.record_peer_failure(
                    &dst,
                    crate::observability::peer_failures::PeerScope::Internal,
                    crate::observability::peer_failures::PeerFailureKind::ResponseTimeout,
                );
            }
        }
        state.ledger.missed(&p.worker_id);
        let n = state.misses.entry(p.worker_id.clone()).or_insert(0);
        *n += 1;
        if *n < self.config.threshold {
            return;
        }
        let late =
            state.ledger.late_within(&p.worker_id, Instant::now(), self.config.slow_window());
        let verdict = match self.health_of(&p.worker_id) {
            Some(WorkerHealth::Dead) => return,
            _ if late => WorkerHealth::Slow,
            _ => WorkerHealth::Dead,
        };
        let blind = self.blind();
        self.control.set_health(&p.worker_id, verdict);
        let after = self.health_of(&p.worker_id);
        self.wrote(&p.worker_id, after, blind, state);
    }
}

async fn sleep_until_or_never(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
