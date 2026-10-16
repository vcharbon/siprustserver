//! The drain trigger: a run drained in the middle of its duration places no
//! further call, lets the calls in flight end, and finishes with every
//! offered call accounted for, all of them answered and released.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::outcome::OutcomeSink;
use loadgen::{Driver, Reporter, ReporterCfg};
use serde_json::Value;

use crate::fake_rig::{cfg, mix, setup_fake};

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_drained_run_ends_its_calls_in_flight_and_places_no_more() {
    let (h, b2bua, core, transport) = setup_fake(7950).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let file = std::env::temp_dir().join(format!("loadgen-drain-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let sink = Arc::new(
        OutcomeSink::new("run-1", "loadgen", "main", &file, None).unwrap().with_call_id_tag("-e1"),
    );
    let mut c = cfg(b2bua.addr, 4.0, 3600, 64, 0xD4A1);
    c.call.talk_time = Duration::from_secs(2);
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport)
        .with_outcomes(sink.clone());
    let rate = driver.rate_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        rate.drain();
    });

    let started = tokio::time::Instant::now();
    driver.run().await;
    let ran = started.elapsed();
    sink.close();
    assert!(ran < Duration::from_secs(60), "the drained run ended in {ran:?}, not at its hour");

    let records: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let _ = std::fs::remove_file(&file);
    let offered = sink.offered_total();
    assert!((15..=25).contains(&offered), "about 5 s at 4 cps offered, got {offered}");
    assert!(sink.reconciles(), "an offered call is unaccounted for");
    assert_eq!(records.len() as u64, offered, "every offered call was placed and recorded");
    for r in &records {
        assert_eq!(r["ending"], "answered", "a call in flight ended cleanly: {r}");
        assert!(r["call_id"].as_str().is_some_and(|c| c.contains(".run-1-e1@")), "{r}");
    }
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
