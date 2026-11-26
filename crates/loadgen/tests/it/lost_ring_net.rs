//! Calls whose caller's ringing the network lost still end on an accepted
//! ending, never by a step timeout: a caller whose 180 is dropped on its way
//! to her CANCELs once the loss model reported the drop and she heard the
//! 100 (RFC 3261 §9.1); a caller whose early INFO would name no early dialog
//! skips it, and the callee still answers.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use callshapes::generator::{
    self, AfterAnswer, Cell, Early, Ending, Sender, Talk, Transferred, Winner,
};
use callshapes::shapes::default_binder;
use loadgen::{
    CallTuning, Driver, DropDir, MixEntry, Reporter, ReporterCfg, ResultClass, TargetedDrop,
};
use scenario_harness::actor::ActorScenario;

use crate::fake_rig::*;

/// `entry`'s calls, each losing the first provisional to an INVITE that
/// `drop` names: every call ends ok.
async fn ringing_lost(base: u16, entry: MixEntry, drop: TargetedDrop) {
    let (h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let mut c = cfg(b2bua.addr, 4.0, 1, 4, 0x10_57);
    c.call.ring_delay = Duration::from_millis(300);
    c.default_tuning = CallTuning { retransmit: true, drop_nth: Some(drop), ..Default::default() };
    let shape = entry.id;
    Driver::new(c, vec![entry], reporter.clone(), transport).run().await;
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;

    let total = reporter.total_calls();
    assert!(total >= 4, "calls ran: {total}");
    let prom = reporter.render_prometheus();
    assert_eq!(reporter.count(shape, &ResultClass::Ok), total, "every call ok:\n{prom}");
    assert!(core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed) >= total);
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The caller's first provisional, lost on its way to her.
fn caller_s_ringing() -> TargetedDrop {
    TargetedDrop::provisional("INVITE", 1, DropDir::Inbound)
}

#[tokio::test(start_paused = true)]
async fn an_abandon_whose_ringing_was_lost_still_cancels() {
    ringing_lost(7720, mix("abandon_ringing", 1.0), caller_s_ringing()).await;
}

#[tokio::test(start_paused = true)]
async fn a_cancel_answer_crossing_whose_ringing_was_lost_still_ends() {
    ringing_lost(7730, mix("cancel_answer_crossing", 1.0), caller_s_ringing()).await;
}

/// A held call whose caller sends early INFOs on the early dialog her
/// unreliable 180 opens: that 180 lost on its way to her, she has no early
/// dialog to name, sends no INFO without a To-tag, and the callee answers.
#[tokio::test(start_paused = true)]
async fn a_caller_whose_ringing_was_lost_skips_her_early_info_and_is_answered() {
    let cell = Cell {
        sender: Sender::Caller,
        early: Early::Info,
        after: AfterAnswer::Bye,
        winner: Winner::Sole,
        reliable: false,
        ending: Ending::CallerBye,
        talk: Talk::Short,
        transfer: Transferred::None,
    };
    let plan: Arc<dyn ActorScenario> = Arc::new(generator::plan(&cell, default_binder()));
    ringing_lost(7740, MixEntry::from((plan, 1.0)), caller_s_ringing()).await;
}
