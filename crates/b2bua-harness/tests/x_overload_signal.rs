//! migration/08 — the worker-side `X-Overload` load signal the front proxy's
//! ELU-band AIMD consumes (port of the X-Overload publish surface of
//! `OverloadController.ts` / `LoadSampler.ts`).
//!
//! Closes the producer→consumer loop the migration item is about: a *running*
//! `B2buaCore` publishes `X-Overload: v=1; elu=…; gc=…; adm=…` and the front
//! proxy's REAL parser (`sip_proxy::load_observer::parse_x_overload_header`,
//! already ported as the consumer) accepts it. The unit-level publish-surface
//! contracts (schema, adm counter, EWMA-starts-at-0, injected-sampler-drives-EWMA)
//! are pinned in `b2bua::overload::tests`; the responder wiring (header on the
//! 200, absent on the 503) in `b2bua::repl::s7_tests`. This file adds the
//! end-to-end + paused-clock-sampler-task pieces those unit tests can't reach.

use std::sync::Arc;
use std::time::Duration;

use b2bua::overload::{simulated, OverloadSignal};
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_proxy::load_observer::parse_x_overload_header;

/// The spawned 100 ms sampler task (b2bua_core) rides `tokio::time`, so advancing
/// the paused clock drives it: after a few sample periods the published header is
/// still a valid `v=1` payload the proxy parser accepts, with EWMAs in `[0,1]`.
/// (The live sampler reports a structurally-0 GC fraction on Rust — no managed
/// GC — and an in-range ELU; the point is the task ran and produced parseable
/// output, not a specific load value.)
#[tokio::test(start_paused = true)]
async fn running_worker_publishes_parseable_x_overload_after_sampling() {
    let h = Harness::new("b2bua-x-overload-signal")
        .describe("a running B2buaCore publishes a proxy-parseable X-Overload signal");
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071)
        .start(&h, "b2bua", "127.0.0.1:5092")
        .await;

    // Before any sample fires the EWMAs are exactly 0 (the zero-state header).
    let header0 = b2bua.overload().x_overload_header_value();
    assert_eq!(header0, "v=1; elu=0.000; gc=0.000; adm=0");
    let parsed0 = parse_x_overload_header(Some(&header0)).expect("proxy must parse zero-state");
    assert_eq!(parsed0.elu, 0.0);
    assert_eq!(parsed0.gc, 0.0);
    assert_eq!(parsed0.adm, 0.0);

    // Advance several 100 ms sampler periods; the task ticks `sample()` each time.
    h.advance(Duration::from_secs(1)).await;

    // The header is still a valid v=1 payload the REAL proxy parser accepts, with
    // EWMAs clamped to [0,1] (closing the producer→consumer contract).
    let header1 = b2bua.overload().x_overload_header_value();
    let parsed1 = parse_x_overload_header(Some(&header1))
        .expect("proxy must parse the worker's X-Overload after sampling");
    assert!((0.0..=1.0).contains(&parsed1.elu), "elu {} out of [0,1]", parsed1.elu);
    assert!((0.0..=1.0).contains(&parsed1.gc), "gc {} out of [0,1]", parsed1.gc);
    // Rust has no managed GC, so the live sampler's gc fraction is structurally 0.
    assert_eq!(parsed1.gc, 0.0);

    let _report = h.finish().await;
}

/// The faithful `start_paused` port of the TS `it.live` "LoadSampler injection
/// drives eluEwma once the sampler fires": inject an `OverloadSignal` over the
/// `simulated()` sampler, set a non-zero ELU through its control, advance several
/// `SAMPLE_PERIOD`s, and prove the injected value flows THROUGH the running 100 ms
/// sampler task into the published `X-Overload` header (`elu` rises above 0 and
/// converges toward the injected reading) — the full
/// injected-value → running-task → EWMA → header loop the live sampler (~0 ELU
/// under a paused runtime) and the unit-level `sample()`-direct test each only
/// cover one half of.
#[tokio::test(start_paused = true)]
async fn injected_sampler_drives_published_elu_through_the_running_task() {
    let h = Harness::new("b2bua-x-overload-injected")
        .describe("an injected overload sampler drives the published X-Overload elu via the task");

    // Build the worker's overload signal over the simulated sampler so the test
    // controls the ELU the running 100 ms task reads, and inject it into the core.
    let (sampler, ctl) = simulated();
    let overload = OverloadSignal::new(Arc::new(sampler));
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071)
        .overload(overload)
        .start(&h, "b2bua", "127.0.0.1:5094")
        .await;

    // Before any sample fires the EWMA is exactly 0 — even though we now drive a
    // non-zero reading, no tick has fed it yet (the EWMA-starts-at-0 contract).
    ctl.set_elu(0.8);
    let header0 = b2bua.overload().x_overload_header_value();
    assert_eq!(header0, "v=1; elu=0.000; gc=0.000; adm=0");
    assert_eq!(parse_x_overload_header(Some(&header0)).unwrap().elu, 0.0);

    // Advance several 100 ms sampler periods; each tick of the running task reads
    // the injected 0.8 and feeds the EWMA. After ~10 ticks it has converged close
    // to 0.8 (alpha 0.2). The first observe seats the EWMA exactly at 0.8, so a
    // single period already lifts it above 0 — but we advance more to prove the
    // task keeps driving the smoothing toward the injected value.
    h.advance(Duration::from_secs(1)).await;

    let header1 = b2bua.overload().x_overload_header_value();
    let parsed1 = parse_x_overload_header(Some(&header1))
        .expect("proxy must parse the worker's X-Overload after injected sampling");
    // The injected value reached the published header THROUGH the running task.
    assert!(
        parsed1.elu > 0.0,
        "injected elu must drive the published elu above 0, got {} (header {header1:?})",
        parsed1.elu,
    );
    // And it has converged toward the injected 0.8 (it seats at 0.8 on the first
    // tick and every subsequent tick re-observes 0.8, so it stays at 0.8).
    assert!(
        (parsed1.elu - 0.8).abs() < 0.05,
        "published elu {} should converge toward the injected 0.8",
        parsed1.elu,
    );
    // gc stays structurally 0 (no GC fraction injected; Rust has no managed GC).
    assert_eq!(parsed1.gc, 0.0);

    let _report = h.finish().await;
}

/// `increment_non_emergency_admitted()` on the running worker advances the `adm`
/// the proxy parses off the published header — the LB's per-worker treated-rate
/// diff input. End-to-end through the real parser.
#[tokio::test(start_paused = true)]
async fn admit_counter_advances_the_parsed_adm() {
    let h = Harness::new("b2bua-x-overload-adm")
        .describe("worker admit counter is visible to the proxy as the parsed adm");
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071)
        .start(&h, "b2bua", "127.0.0.1:5093")
        .await;

    let before = parse_x_overload_header(Some(&b2bua.overload().x_overload_header_value()))
        .unwrap()
        .adm;
    assert_eq!(before, 0.0);

    for _ in 0..5 {
        b2bua.overload().increment_non_emergency_admitted();
    }

    let after = parse_x_overload_header(Some(&b2bua.overload().x_overload_header_value()))
        .unwrap()
        .adm;
    assert_eq!(after, before + 5.0, "proxy-parsed adm must track the worker's admits");

    let _report = h.finish().await;
}
