//! ADR-0020 X2 — the terminal `RemoveCall` is interpreted AFTER the buffered
//! `WriteCdr`, so the CDR is enqueued while the call (and its replicated
//! Element) still exists. Regression guard for the old lane order, where the
//! eviction — and the propagated replica delete — ran before the CDR was even
//! enqueued, so a failure in that window lost the CDR everywhere.
//!
//! The probe: a CDR tap that samples the SUT's live-call count at write
//! time. New order → the call is still resident (`1`); the old order would
//! observe `0`.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter};
use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{establish, settle_until, B2buaSut};
use scenario_harness::Harness;

/// The live-call count the SUT hands out once it runs.
type LiveCalls = Arc<OnceLock<Arc<dyn Fn() -> usize + Send + Sync>>>;

/// Samples the SUT's live-call count on every CDR write.
struct ProbeCdr {
    live_at_write: Arc<Mutex<Vec<usize>>>,
    live_calls: LiveCalls,
}

#[async_trait]
impl CdrWriter for ProbeCdr {
    async fn write(&self, _call: &call::Call, _terminated_at: i64) {
        if let Some(live) = self.live_calls.get() {
            self.live_at_write.lock().unwrap().push(live());
        }
    }
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
}

#[tokio::test]
async fn cdr_is_written_while_the_call_is_still_live() {
    let h = Harness::with_transit_delay("cdr-before-remove", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let live_at_write = Arc::new(Mutex::new(Vec::new()));
    let live_calls: LiveCalls = Arc::new(OnceLock::new());
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)))
            .cdr_tap(Arc::new(ProbeCdr {
                live_at_write: live_at_write.clone(),
                live_calls: live_calls.clone(),
            }))
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
    let _ = live_calls.set(b2bua.active_calls_probe());

    // Establish + tear down a canonical call.
    let mut dialog = establish(&alice, &bob, b2bua.addr).await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    assert_eq!(
        *live_at_write.lock().unwrap(),
        vec![1],
        "the CDR must be written BEFORE the terminal RemoveCall evicts the call \
         (ADR-0020 X2 lane order)"
    );
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
