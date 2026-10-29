//! The callflow samples a run stores, end to end on the paused-clock rig.

use std::sync::Arc;

use b2bua_harness::settle_until;
use loadgen::{Driver, Reporter, ReporterCfg, ResultClass};

use crate::fake_rig::{cfg, mix, setup_fake_tuned};

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

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
