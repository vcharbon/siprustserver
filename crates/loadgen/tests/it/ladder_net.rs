//! The message ladder on the fake network: retransmissions received and
//! re-sent under loss with the retransmit engine, and a failure charged to
//! the message it was detected at.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::{CallTuning, Canaries, Driver, LoadRunMeta, Reporter, ReporterCfg, ResultClass};

use crate::fake_rig::*;

/// Under 8 % loss with the retransmit engine, the basic-call ladder counts
/// copies received after the first and copies re-sent, on its arrows and in
/// the run's headline.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_lossy_run_ladder_counts_retransmissions_both_ways() {
    let (h, b2bua, core, transport) = setup_fake(7620).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let mut c = cfg(b2bua.addr, 12.0, 3, 8, 0x5EED);
    c.default_tuning = CallTuning { drop_rate: 0.08, retransmit: true, ..CallTuning::default() };
    Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport).run().await;

    let idx = reporter.build_index(LoadRunMeta::default(), Canaries::default());
    let ladder = idx.ladders.iter().find(|l| l.scenario == "basic_call").expect("a ladder");
    assert_eq!(ladder.calls, reporter.total_calls(), "every call folded");
    assert!(ladder.engine);
    assert_eq!(ladder.lanes[..2], ["alice".to_string(), "SUT".to_string()]);
    let invite = &ladder.arrows[0];
    assert_eq!((invite.from.as_str(), invite.label.as_str(), invite.nth), ("alice", "INVITE", 1));
    assert_eq!(invite.count, ladder.calls, "every call sent one INVITE");
    let received: u64 = ladder.arrows.iter().map(|a| a.retransmissions).sum();
    let resent: u64 = ladder.arrows.iter().map(|a| a.resent).sum();
    assert!(received > 0 && resent > 0, "retr {received}, resent {resent}: {ladder:#?}");
    let p = &idx.progress;
    assert_eq!((p.retransmissions_received, p.retransmissions_resent), (received, resent));
    assert!(p.retransmit_engine);

    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// A CPS bucket of one token that never refills: spent, it refuses every new call.
fn one_token(c: &mut b2bua_sdk::B2buaConfig) {
    c.cps_bucket_size = 1;
    c.cps_bucket_rate = 0;
}

/// An SUT that refuses every INVITE with 503: each failed call is charged to
/// the 503 the caller received, the message its failure was detected at.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_refused_call_is_charged_to_the_refusal() {
    let (_h, b2bua, core, transport) = setup_fake_tuned(7630, one_token).await;
    b2bua.overload().spend_token();
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 20.0, 1, 16, 0x503),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.count("basic_call", &ResultClass::WrongStatus(503));
    assert!(total >= 10 && total == reporter.total_calls(), "every call refused: {total}");
    let idx = reporter.build_index(LoadRunMeta::default(), Canaries::default());
    let ladder = &idx.ladders[0];
    let charged: Vec<_> = ladder.arrows.iter().filter(|a| !a.errors.is_empty()).collect();
    assert_eq!(charged.len(), 1, "one arrow carries the failures: {ladder:#?}");
    let refusal = charged[0];
    assert_eq!((refusal.from.as_str(), refusal.to.as_str()), ("SUT", "alice"));
    assert_eq!(refusal.label, "503 (INVITE)");
    assert_eq!(refusal.errors[0].class, "status_503");
    assert_eq!(refusal.errors[0].count, total);
    assert!(ladder.before_reply.is_empty() && ladder.after_call.is_empty());

    settle_until(|| core.registry_size() == 0).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
