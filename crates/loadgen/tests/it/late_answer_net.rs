//! A server transaction outlives its call (RFC 3261 §17.2.2): the callee's
//! 200 to the BYE is lost after the call ended, the SUT retransmits its BYE,
//! and the mux answers it again with the same 200. The repair is counted, on
//! the mux and on the BYE's arrow of the ladder, and is no orphan.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::{
    CallTuning, Canaries, Driver, DropDir, LoadRunMeta, Reporter, ReporterCfg, ResultClass,
    TargetedDrop,
};

use crate::fake_rig::*;

/// Every basic call loses bob's first 200 (BYE) before the wire; the call
/// is already over when the SUT's Timer E re-sends the BYE.
async fn bob_bye_answer_lost(base: u16, retransmit: bool) {
    let (h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let mut c = cfg(b2bua.addr, 4.0, 2, 4, 0xB1E);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit,
        drop_nth: Some(TargetedDrop::final_response("BYE", 1, DropDir::Outbound).on_leg("bob")),
    };
    Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport).run().await;
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 4, "calls ran: {total}");
    assert_eq!(reporter.count("basic_call", &ResultClass::Ok), total, "every call ok:\n{prom}");
    let s = core.stats();
    assert_eq!(s.dropped_out.load(Relaxed), total, "one 200 (BYE) lost per call");
    assert_eq!(s.orphans_total(), 0, "no orphan: {:?}", s.samples());
    let sut = b2bua.metrics().prometheus_text();
    assert!(!sut.contains("kind=\"response_timeout\""), "the SUT's BYE completed:\n{sut}");
    assert_eq!(s.late_answered.load(Relaxed), total, "each retransmitted BYE answered again");
    assert!(
        core.render_prometheus().contains(&format!(
            "loadgen_mux_late_retransmission_answered_total{{stream=\"main\"}} {total}"
        )),
        "exported:\n{}",
        core.render_prometheus()
    );

    let idx = reporter.build_index(LoadRunMeta::default(), Canaries::default());
    let ladder = idx.ladders.iter().find(|l| l.scenario == "basic_call").expect("a ladder");
    let bye = ladder
        .arrows
        .iter()
        .find(|a| (a.from.as_str(), a.to.as_str(), a.label.as_str()) == ("SUT", "bob", "BYE"))
        .expect("bob's BYE arrow");
    assert_eq!(bye.late_answered, total, "on the BYE's arrow: {ladder:#?}");
    let elsewhere: u64 = ladder.arrows.iter().map(|a| a.late_answered).sum();
    assert_eq!(elsewhere, total, "and nowhere else");
    assert_eq!(reporter.tick().late_answered, total, "in the run's first window");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_retransmitted_after_the_call_is_answered_again() {
    bob_bye_answer_lost(7700, false).await;
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_retransmitted_after_the_call_is_answered_again_with_the_engine() {
    bob_bye_answer_lost(7710, true).await;
}
