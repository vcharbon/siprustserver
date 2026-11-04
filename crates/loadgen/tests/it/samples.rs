//! What a run records, stores as callflow samples and judges, end to end on
//! the paused-clock rig.

use std::sync::Arc;

use b2bua_harness::settle_until;
use loadgen::{Driver, Reporter, ReporterCfg, ResultClass};

use crate::fake_rig::{cfg, mix, setup_fake, setup_fake_tuned};

/// An error bucket that first appears after the first `sample_cap` calls
/// stores only recorded calls: an SUT whose admission bucket runs dry after
/// six calls refuses every later one with 503, mostly on calls the 1-in-8
/// share does not record; the bucket still fills with full callflows.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_error_samples_are_recorded_calls() {
    let (_h, b2bua, core, transport) = setup_fake_tuned(7430, |c| {
        c.cps_bucket_size = 6;
        c.cps_bucket_rate = 0;
    })
    .await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 20.0, 2, 16, 0x5035),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let refused = ResultClass::WrongStatus(503);
    let prom = reporter.render_prometheus();
    assert!(reporter.count("basic_call", &refused) >= 20, "too few refusals:\n{prom}");
    assert_eq!(reporter.stub_sample_count("basic_call", &refused), 0, "a stub took a slot");
    assert_eq!(reporter.sample_count("basic_call", &refused), 3, "the bucket fills:\n{prom}");
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    assert_eq!(reporter.accepted_count("basic_call"), ok, "only accepted calls are audited");
    assert!(reporter.audited_count("basic_call") <= ok, "only recorded ones by their trace");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// A declared reject end to end: every `invite_reject` call ends on its
/// `rejected` branch and classes `ok` after the sampled RFC audit (an accepted
/// branch is still audited: the caller ACKed the 486 before the call settled,
/// so the recorded flow is complete), and the round trips the mux measured
/// reach `loadgen_rtt_seconds` once each call ends.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_accepted_reject_is_audited_clean() {
    let (_h, b2bua, core, transport) = setup_fake(7300).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 50, background_record_every: 1 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 20.0, 1, 16, 0x486),
        vec![mix("invite_reject", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 10, "governor under-delivered on virtual time: {total}");
    assert_eq!(reporter.count("invite_reject", &ResultClass::Ok), total, "{prom}");
    assert_eq!(reporter.audited_count("invite_reject"), total, "every recorded ok call is audited");
    let line =
        format!("loadgen_branch_total{{stream=\"main\",shape=\"invite_reject\",branch=\"rejected\"}} {total}");
    assert!(prom.lines().any(|l| l == line), "expected `{line}` in:\n{prom}");
    assert!(
        prom.contains(
            "loadgen_rtt_seconds_count{stream=\"main\",shape=\"invite_reject\",exchange=\"invite_100\"}"
        ),
        "{prom}"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
