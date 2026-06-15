//! ADR-0020 X2 — the terminal `RemoveCall` is interpreted AFTER the buffered
//! `WriteCdr`, so the CDR is enqueued while the call (and its replicated
//! Element) still exists. Regression guard for the old lane order, where the
//! eviction — and the propagated replica delete — ran before the CDR was even
//! enqueued, so a failure in that window lost the CDR everywhere.
//!
//! The probe: a [`CdrWriter`] that samples the SUT's live-call count at write
//! time. New order → the call is still resident (`1`); the old order would
//! observe `0`.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use b2bua_harness::{establish, settle_until};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_txn::IdGen;

/// Samples `live_calls()` (set post-spawn) on every CDR write.
struct ProbeCdr {
    inner: InMemoryCdrWriter,
    live_at_write: Arc<Mutex<Vec<usize>>>,
    live_calls: Arc<OnceLock<Box<dyn Fn() -> usize + Send + Sync>>>,
}

#[async_trait]
impl CdrWriter for ProbeCdr {
    async fn write(&self, call: &call::Call, terminated_at: i64) {
        if let Some(live) = self.live_calls.get() {
            self.live_at_write.lock().unwrap().push(live());
        }
        self.inner.write(call, terminated_at).await;
    }
    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }
}

#[tokio::test]
async fn cdr_is_written_while_the_call_is_still_live() {
    let h = Harness::with_transit_delay("cdr-before-remove", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Manual SUT (instead of `B2buaSut`) so the probe CdrWriter is injected.
    let (endpoint, sa) = h.bind_sut("b2bua", "127.0.0.1:5080").await;
    let inner = InMemoryCdrWriter::new();
    let live_at_write = Arc::new(Mutex::new(Vec::new()));
    let live_calls: Arc<OnceLock<Box<dyn Fn() -> usize + Send + Sync>>> =
        Arc::new(OnceLock::new());
    let deps = B2buaDeps {
        config: B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: sa.ip().to_string(),
            sip_local_port: sa.port(),
            ..Default::default()
        },
        decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(ProbeCdr {
            inner: inner.clone(),
            live_at_write: live_at_write.clone(),
            live_calls: live_calls.clone(),
        }),
        store: Arc::new(InMemoryCallStore::new()),
        clock: Clock::test_at(0),
        id_gen: Arc::new(IdGen::seeded(0xB2B0)),
        replication: None,
        metrics: B2buaMetrics::new(),
    };
    let core = Arc::new(B2buaCore::spawn(endpoint, deps));
    let probe_core = core.clone();
    let _ = live_calls.set(Box::new(move || probe_core.active_calls()));

    // Establish + tear down a canonical call.
    let mut dialog = establish(&alice, &bob, sa).await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| inner.snapshot().len() == 1).await;
    assert_eq!(inner.snapshot().len(), 1, "exactly one CDR");
    assert_eq!(
        *live_at_write.lock().unwrap(),
        vec![1],
        "the CDR must be written BEFORE the terminal RemoveCall evicts the call \
         (ADR-0020 X2 lane order)"
    );
    settle_until(|| core.active_calls() == 0).await;
    assert_eq!(core.active_calls(), 0, "the call is still removed afterwards");

    let _report = h.finish().await;
}
