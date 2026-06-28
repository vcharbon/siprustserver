//! The load driver: a CPS governor that spawns one `Send` task per call onto a
//! shared multi-threaded runtime, bounded by a max-in-flight semaphore, picking
//! scenarios by weighted random. Each per-call task mints a correlation token,
//! binds its agents on the **mux** (one socket per defined endpoint, many
//! dialogs demuxed), runs the scenario inside a `catch_unwind` boundary (a panic
//! is a *counted* failure, never a worker abort), tears the call down
//! (CANCEL/BYE) however it ended, classifies the result, and records it —
//! optionally projecting a sampled callflow (recording layered on the mux).

use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use scenario_harness::AgentBinder;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::class::{CallOutcome, ResultClass};
use crate::ctx::{CallCtx, CallEnv};
use crate::mux::{CallRouting, Correlation, MuxCore};
use crate::report::{RenderedSample, Reporter};
use crate::scenarios::LoadScenario;
use crate::scope::CallScope;

/// The mux transport the driver binds calls on.
pub struct MuxTransport {
    pub core: Arc<MuxCore>,
    /// The caller (UAC) endpoint address.
    pub uac_addr: SocketAddr,
    /// The callee (UAS) endpoint address (the SUT routes the b-leg here).
    pub uas_addr: SocketAddr,
    /// The transfer-target (REFER) endpoint address.
    pub refer_addr: SocketAddr,
    /// How the correlation token travels through the SUT.
    pub correlation: Correlation,
    /// Per-recv wall-clock timeout.
    pub recv_timeout: Duration,
}

/// Static per-call routing config (shared `Arc` across all calls).
pub struct CallConfig {
    /// The address the initial INVITE routes through (SUT / VIP).
    pub via: SocketAddr,
    /// Optional `X-Api-Call` destination pin → the static `uas` endpoint
    /// (our-b2bua routing adapter). `None` when the SUT routes the callee itself.
    pub route_pin: Option<SocketAddr>,
    /// Optional `X-Api-Call` REFER destination pin → the static `refer` endpoint.
    pub refer_pin: Option<SocketAddr>,
    pub refer_key: String,
    pub options_hold: Duration,
    pub options_cadence: Duration,
    /// Realistic ring time (callee 180→200 dwell). `0` = answer immediately.
    pub ring_delay: Duration,
    /// Post-connect talk time before BYE (basic call). `0` = hang up immediately.
    pub talk_time: Duration,
    /// Spacing held before and after a re-INVITE. `0` = back-to-back.
    pub reinvite_gap: Duration,
    /// Total hold of a long recorded call, split either side of its OPTIONS ping.
    pub long_hold: Duration,
    /// After a *failed* call's a-leg is torn down, how long to drain-and-200 the
    /// in-process callee legs so the SUT closes its relayed b-leg promptly.
    pub teardown_quiesce: Duration,
}

/// Driver construction config.
pub struct DriverCfg {
    pub cps: f64,
    pub duration: Duration,
    pub max_in_flight: usize,
    pub seed: u64,
    pub call: CallConfig,
}

/// The load driver.
pub struct Driver {
    cps: f64,
    duration: Duration,
    max_in_flight: usize,
    seed: u64,
    reporter: Arc<Reporter>,
    scenarios: Vec<(Arc<dyn LoadScenario>, f64)>,
    total_weight: f64,
    sem: Arc<Semaphore>,
    transport: Arc<MuxTransport>,
    call: Arc<CallConfig>,
}

/// Process-wide per-call id-seed source: a unique 100k-wide window per call so a
/// fresh binder (whose `Ids` restart at 1) never mints Call-IDs colliding with
/// another in-flight or prior call against the same stateful SUT.
fn next_seed(base: u64) -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    base.wrapping_add(SERIAL.fetch_add(1, Ordering::Relaxed).wrapping_mul(100_000))
        .max(1)
}

/// A random per-call correlation token, formatted as a valid SIP user-part.
fn mint_token() -> String {
    format!("lg{}", uuid::Uuid::new_v4().simple())
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

impl Driver {
    pub fn new(
        cfg: DriverCfg,
        scenarios: Vec<(Arc<dyn LoadScenario>, f64)>,
        reporter: Arc<Reporter>,
        transport: Arc<MuxTransport>,
    ) -> Self {
        assert!(!scenarios.is_empty(), "loadgen needs at least one scenario");
        let total_weight = scenarios.iter().map(|(_, w)| *w).sum();
        Self {
            cps: cfg.cps,
            duration: cfg.duration,
            max_in_flight: cfg.max_in_flight,
            seed: cfg.seed.max(1),
            reporter,
            scenarios,
            total_weight,
            sem: Arc::new(Semaphore::new(cfg.max_in_flight)),
            transport,
            call: Arc::new(cfg.call),
        }
    }

    pub fn reporter(&self) -> &Arc<Reporter> {
        &self.reporter
    }

    fn pick(&self, rng: &mut u64) -> Arc<dyn LoadScenario> {
        let r = (xorshift(rng) as f64 / u64::MAX as f64) * self.total_weight;
        let mut acc = 0.0;
        for (s, w) in &self.scenarios {
            acc += *w;
            if r <= acc {
                return s.clone();
            }
        }
        self.scenarios.last().unwrap().0.clone()
    }

    /// Run the load for the configured duration, then drain in-flight calls.
    pub async fn run(&self) {
        // Fixed-grid CPS governor. The nth call is scheduled at the ABSOLUTE slot
        // `start + n*period`, anchored at the run start. We sleep until that
        // instant; if it is already in the past — we fell behind on wake/scheduling
        // jitter — the remaining delay is <= 0, so we DO NOT sleep (never a negative
        // or zero sleep) and fire immediately, catching the slip back up. This holds
        // the long-run offered rate at exactly `cps`.
        //
        // The previous `tokio::time::interval` + `MissedTickBehavior::Delay`
        // re-anchored the next deadline to each (late) wake instead of catching up,
        // so a few-ms per-tick scheduling latency was permanently shed every cycle —
        // ~8% under endurance load (20 cps offered as ~18.4). The grid below cannot
        // accumulate that drift. A catch-up burst (after a long stall) is naturally
        // bounded by the max-in-flight semaphore — the excess is shed+counted, never
        // an unbounded spawn storm.
        let period = Duration::from_secs_f64(1.0 / self.cps);
        let start = tokio::time::Instant::now();
        let mut rng = self.seed;
        let mut n: u64 = 0;

        loop {
            // Offset of the nth slot from `start`. Computed in Duration space and
            // compared to `self.duration` (not as an Instant) so a huge n can never
            // overflow `Instant` arithmetic — once the slot reaches the window end
            // we stop (total offered ≈ cps × duration).
            let offset = match u32::try_from(n).ok().and_then(|k| period.checked_mul(k)) {
                Some(off) if off < self.duration => off,
                _ => break,
            };
            let target = start + offset;

            // Sleep ONLY while ahead of the slot; on/behind it (delay <= 0) fire now.
            let now = tokio::time::Instant::now();
            if target > now {
                tokio::time::sleep_until(target).await;
            }
            n += 1;

            let scenario = self.pick(&mut rng);
            let permit = match self.sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    self.reporter.inc_shed(scenario.id());
                    continue;
                }
            };
            tokio::spawn(run_one(
                scenario,
                self.reporter.clone(),
                self.transport.clone(),
                self.call.clone(),
                self.seed,
                permit,
            ));
        }

        // Drain: acquiring every permit blocks until all in-flight calls release.
        let _ = self.sem.acquire_many(self.max_in_flight as u32).await;
    }
}

/// One call, start to finish. `Send + 'static`, so it runs on the shared
/// multi-threaded runtime.
async fn run_one(
    scenario: Arc<dyn LoadScenario>,
    reporter: Arc<Reporter>,
    transport: Arc<MuxTransport>,
    call: Arc<CallConfig>,
    seed_base: u64,
    _permit: OwnedSemaphorePermit,
) {
    reporter.inc_inflight();
    let id = scenario.id();

    // One correlation token per CALL: alice stamps it on her INVITE and the SUT
    // relays it (the transparent header) onto every downstream leg, so bob and
    // charlie share it. Each callee leg is declared on its own socket; the mux
    // demuxes by (socket, token) — a single receiver per socket here, so no
    // picker is needed (the scenario-owned picker is for >1 receiver per socket).
    let token = mint_token();
    let mut routing = CallRouting::new(token.clone()).leg(transport.uas_addr, "bob");
    if scenario.needs_charlie() {
        routing = routing.leg(transport.refer_addr, "charlie");
    }

    let record = reporter.should_record(id);
    let mux_net = transport.core.network(routing);
    let binder = AgentBinder::mux(Arc::new(mux_net), transport.recv_timeout, record);
    binder.seed_ids(next_seed(seed_base));

    let alice = binder.agent("alice", &transport.uac_addr.to_string()).await;
    let bob = binder.agent("bob", &transport.uas_addr.to_string()).await;
    let charlie = if scenario.needs_charlie() {
        Some(binder.agent("charlie", &transport.refer_addr.to_string()).await)
    } else {
        None
    };

    let env = CallEnv {
        alice: &alice,
        bob: &bob,
        charlie: charlie.as_ref(),
        via: call.via,
        correlation_header: transport.correlation.header_name().to_string(),
        token,
        emergency: scenario.emergency(),
        route_pin: call.route_pin,
        refer_pin: call.refer_pin,
        refer_key: call.refer_key.clone(),
        options_hold: call.options_hold,
        options_cadence: call.options_cadence,
        ring_delay: call.ring_delay,
        talk_time: call.talk_time,
        reinvite_gap: call.reinvite_gap,
        long_hold: call.long_hold,
    };
    let scope = CallScope::new();
    let ctx = CallCtx::new();

    let result = AssertUnwindSafe(scenario.run(&env, &scope, &ctx)).catch_unwind().await;

    // Cleanup FIRST (release any dialog on the SUT), then classify/report.
    scope.teardown().await;
    let failed = !matches!(result, Ok(Ok(())));
    if failed && !call.teardown_quiesce.is_zero() {
        bob.quiesce(call.teardown_quiesce).await;
        if let Some(c) = &charlie {
            c.quiesce(call.teardown_quiesce).await;
        }
    }

    let outcome = match result {
        Ok(Ok(())) => {
            let findings = binder.rfc_findings();
            if findings.is_empty() {
                CallOutcome::Ok
            } else {
                CallOutcome::RfcAuditFail(
                    findings.iter().map(|f| f.detail.clone()).collect::<Vec<_>>().join("; "),
                )
            }
        }
        Ok(Err(e)) => CallOutcome::Step(e),
        Err(payload) => CallOutcome::Panic(panic_msg(payload)),
    };

    let class = ResultClass::from(&outcome);
    let e2e = ctx.elapsed();
    let checkpoints = ctx.take_checkpoints();

    let sample = if reporter.wants_sample(id, &class) {
        let detail = outcome.detail();
        // Thread the failure reason into the rendered callflow so a sampled NOK
        // page explains WHY (header banner + an explicit anomaly), not just "FAIL".
        let html = if binder.is_recording() {
            binder.render_html(id, class.is_ok(), detail.as_deref())
        } else {
            None
        };
        if html.is_some() || !class.is_ok() {
            Some(RenderedSample {
                html,
                detail,
                e2e_ms: e2e.as_secs_f64() * 1000.0,
            })
        } else {
            None
        }
    } else {
        None
    };

    reporter.record(id, &outcome, e2e, &checkpoints, sample);
    reporter.dec_inflight();
}

/// A minimal Prometheus `/metrics` HTTP server (hand-rolled HTTP/1.1 over
/// `TcpListener` — no hyper dependency). Serves `render()` to any GET. Runs until
/// the task is cancelled.
pub async fn serve_metrics(
    addr: SocketAddr,
    render: Arc<dyn Fn() -> String + Send + Sync>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (mut sock, _) = listener.accept().await?;
        let render = render.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = render();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
