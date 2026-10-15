//! Outcome records end to end on the paused-clock rig: every call the driver
//! placed writes one `call-outcome.v1` record stating what its caller and its
//! callee legs saw, and the records plus the calls never placed account for
//! every call the reporter counted.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::outcome::OutcomeSink;
use loadgen::{Driver, Reporter, ReporterCfg};
use serde_json::Value;

use crate::fake_rig::{cfg, mix, setup_fake};

fn read_records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON record per line"))
        .collect()
}

/// The cleanup-coverage mix: an answered call, a callee's 486, a caller's
/// CANCEL after ringing and a declined transfer each write the record of what
/// the wire showed, with the callee legs loadgen played.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn every_placed_call_writes_the_record_of_its_wire() {
    let (h, b2bua, core, transport) = setup_fake(7800).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let file = std::env::temp_dir().join(format!("loadgen-records-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let sink = Arc::new(OutcomeSink::new("run-1", "loadgen", "main", &file, None).unwrap());
    let shapes = ["basic_call", "invite_reject", "abandon_ringing"];
    let mut c = cfg(b2bua.addr, 20.0, 2, 16, 0x0C0E);
    c.call.ring_delay = Duration::from_millis(500);
    let driver =
        Driver::new(c, shapes.iter().map(|s| mix(s, 1.0)).collect(), reporter.clone(), transport)
            .with_outcomes(sink.clone());
    driver.run().await;
    sink.close();
    let records = read_records(&file);
    let _ = std::fs::remove_file(&file);

    let total = reporter.total_calls();
    assert!(total >= 20, "governor under-delivered on virtual time: {total}");
    assert_eq!(records.len() as u64 + sink.not_placed_total(), total, "every call is accounted");
    assert_eq!(sink.not_placed_total(), 0);

    let mut endings: BTreeMap<(String, String), u64> = BTreeMap::new();
    for r in &records {
        assert_eq!(r["schema"], "call-outcome.v1");
        assert_eq!((r["run"].as_str(), r["stream"].as_str()), (Some("run-1"), Some("main")));
        assert!(r["call_id"].as_str().is_some_and(|c| !c.is_empty()));
        assert!(r["started_at_ms"].as_i64() <= r["ended_at_ms"].as_i64());
        let key =
            (r["shape"].as_str().unwrap().to_string(), r["ending"].as_str().unwrap().to_string());
        *endings.entry(key).or_default() += 1;
        match r["ending"].as_str().unwrap() {
            "answered" => {
                assert_eq!(r["phase_reached"], "released", "{r}");
                assert_eq!(r["legs"][0]["ending"], "answered", "{r}");
            }
            "rejected" => {
                assert_eq!(
                    (r["final_code"].as_u64(), r["reason"].as_str()),
                    (Some(486), Some("486")),
                    "{r}"
                );
                assert_eq!(r["legs"][0]["ending"], "rejected", "{r}");
            }
            "abandoned" => {
                assert_eq!(r["final_code"], 487, "{r}");
                assert_eq!(r["legs"][0]["ending"], "abandoned", "{r}");
            }
            other => panic!("unexpected ending {other}: {r}"),
        }
    }
    let shapes_ending: Vec<(&str, &str)> =
        endings.keys().map(|(s, e)| (s.as_str(), e.as_str())).collect();
    assert_eq!(
        shapes_ending,
        [
            ("abandon_ringing", "abandoned"),
            ("basic_call", "answered"),
            ("invite_reject", "rejected")
        ],
        "{endings:?}"
    );

    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
