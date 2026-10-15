//! Outcome records end to end on the paused-clock rig: every call the driver
//! offered is accounted for (a record, a call never placed, or a generator
//! failure), and each record states what its caller and its callee legs saw,
//! sealed when the call's verdict resolved.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use loadgen::outcome::OutcomeSink;
use loadgen::{
    CallTuning, Driver, DriverCfg, DropDir, MuxCore, MuxTransport, Reporter, ReporterCfg,
    TargetedDrop,
};
use scenario_harness::Harness;
use serde_json::Value;

use crate::fake_rig::{addr, cfg, mix, setup_fake, setup_fake_faulty};

type Rig = (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>);

/// Drive `shapes` with `tune` over `rig`; the records written, after the SUT
/// is reaped and every offered call is accounted for.
async fn run(rig: Rig, shapes: &[&str], tune: impl FnOnce(&mut DriverCfg)) -> Vec<Value> {
    let (h, b2bua, core, transport) = rig;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let file = std::env::temp_dir().join(format!(
        "loadgen-records-{}-{}.jsonl",
        std::process::id(),
        shapes.join("-")
    ));
    let _ = std::fs::remove_file(&file);
    let sink = Arc::new(OutcomeSink::new("run-1", "loadgen", "main", &file, None).unwrap());
    let mut c = cfg(b2bua.addr, 4.0, 2, 16, 0x0C0E);
    tune(&mut c);
    let driver =
        Driver::new(c, shapes.iter().map(|s| mix(s, 1.0)).collect(), reporter.clone(), transport)
            .with_outcomes(sink.clone());
    driver.run().await;
    sink.close();
    let records: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON record per line"))
        .collect();
    let _ = std::fs::remove_file(&file);

    assert!(sink.offered_total() > 0, "no call offered");
    assert!(sink.reconciles(), "an offered call is unaccounted for");
    assert_eq!(records.len() as u64, sink.records_written());
    assert_eq!(sink.generator_failures_total(), 0);
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    records
}

/// The fields every record states the same way, whatever its ending.
fn assert_common(r: &Value) {
    assert_eq!(r["schema"], "call-outcome.v1");
    assert_eq!((r["run"].as_str(), r["stream"].as_str()), (Some("run-1"), Some("main")));
    assert!(r["call_id"].as_str().is_some_and(|c| c.contains(".run-1@")), "the run nonce: {r}");
    let started = r["started_at_ms"].as_i64().unwrap();
    let ended = r["ended_at_ms"].as_i64().unwrap();
    assert!(started <= ended, "{r}");
    for at in ["first_provisional_at_ms", "setup_ended_at_ms", "answered_at_ms"] {
        if let Some(t) = r[at].as_i64() {
            assert!(started <= t && t <= ended, "{at} outside the call: {r}");
        }
    }
    if let (Some(p), Some(s)) =
        (r["first_provisional_at_ms"].as_i64(), r["setup_ended_at_ms"].as_i64())
    {
        assert!(p <= s, "{r}");
    }
}

/// The answered, rejected and abandoned shapes write the record of their
/// wire: the ending, the final and its reason, every instant, the callee leg,
/// no failure, and loadgen's own class in `producer_verdict` only.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn every_placed_call_writes_the_record_of_its_wire() {
    let shapes = ["basic_call", "invite_reject", "abandon_ringing"];
    let records = run(setup_fake(7800).await, &shapes, |c| {
        c.call.ring_delay = Duration::from_millis(500);
    })
    .await;
    let mut endings: BTreeMap<(String, String), u64> = BTreeMap::new();
    for r in &records {
        assert_common(r);
        assert_eq!(r["failures"], serde_json::json!([]), "{r}");
        assert_eq!(r["early_media"], false, "{r}");
        let key =
            (r["shape"].as_str().unwrap().to_string(), r["ending"].as_str().unwrap().to_string());
        *endings.entry(key).or_default() += 1;
        let (verdict, final_code, reason, leg) = match r["ending"].as_str().unwrap() {
            "answered" => {
                assert_eq!(r["phase_reached"], "released", "{r}");
                assert_eq!(r["answered_at_ms"], r["setup_ended_at_ms"], "{r}");
                ("ok:answered", 200, Value::Null, "answered")
            }
            "rejected" => ("ok:rejected", 486, "486".into(), "rejected"),
            "abandoned" => ("ok:abandoned", 487, "487".into(), "abandoned"),
            other => panic!("unexpected ending {other}: {r}"),
        };
        assert_eq!(r["producer_verdict"], verdict, "{r}");
        assert_eq!((r["final_code"].as_u64(), &r["reason"]), (Some(final_code), &reason), "{r}");
        assert_eq!(r["legs"][0]["ending"], leg, "{r}");
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
}

/// The SUT stalls after its 100 (the callee never gets the INVITE): the
/// caller's wait for the 180 times out, its teardown CANCELs, and the 487
/// that draws comes after the verdict. The record is `timeout`, never the
/// `abandoned` a CANCELing shape declares, never a 487.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_stalled_setup_is_a_timeout_whatever_its_teardown_draws() {
    let records = run(setup_fake(7810).await, &["basic_call", "abandon_ringing"], |c| {
        c.default_tuning = CallTuning {
            drop_nth: Some(TargetedDrop {
                method: "INVITE",
                nth: 1,
                permanent: true,
                dir: DropDir::Inbound,
                leg: Some("bob"),
            }),
            ..CallTuning::default()
        };
    })
    .await;
    assert!(!records.is_empty());
    for r in &records {
        assert_common(r);
        assert_eq!(
            (r["ending"].as_str(), r["reason"].as_str()),
            (Some("timeout"), Some("timeout")),
            "{r}"
        );
        assert_eq!(
            (r["final_code"].clone(), r["answered_at_ms"].clone()),
            (Value::Null, Value::Null),
            "{r}"
        );
        assert_eq!(r["setup_ended_at_ms"], r["ended_at_ms"], "{r}");
        assert_eq!(r["producer_verdict"], "timeout", "{r}");
        assert_eq!(r["legs"], serde_json::json!([]), "the callee leg never opened: {r}");
    }
}

/// The caller's BYE never reaches the SUT and draws no final: `stranded`,
/// the dialog never released.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_without_a_final_is_stranded() {
    let records = run(setup_fake(7820).await, &["basic_call"], |c| {
        c.default_tuning = CallTuning {
            drop_nth: Some(TargetedDrop {
                method: "BYE",
                nth: 1,
                permanent: true,
                dir: DropDir::Outbound,
                leg: None,
            }),
            ..CallTuning::default()
        };
    })
    .await;
    assert!(!records.is_empty());
    for r in &records {
        assert_common(r);
        assert_eq!(
            (r["ending"].as_str(), r["phase_reached"].as_str()),
            (Some("stranded"), Some("answered")),
            "{r}"
        );
        assert_eq!(r["final_code"], 200, "{r}");
        assert_eq!(r["legs"][0]["ending"], "answered", "{r}");
    }
}

/// A send the fabric refuses from the caller to the SUT (a peer gone, an ICMP
/// refusal) is a transport failure, never a generator one: refused on the
/// INVITE, the call is `failed` before answer with reason `transport`;
/// refused on the BYE, `failed` after answer with a `transport` failure.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_refused_send_fails_the_call() {
    for (base, refused_send, answered) in [(7830u16, 1u32, false), (7840, 3, true)] {
        let caller: SocketAddr = addr(base);
        let sends = Arc::new(AtomicU32::new(0));
        let counted = sends.clone();
        let fault: sip_net::SendFault = Arc::new(move |src, _dst| {
            (src == caller && counted.fetch_add(1, Ordering::SeqCst) + 1 == refused_send)
                .then(|| "refused by the test fabric".to_string())
        });
        let records = run(setup_fake_faulty(base, fault).await, &["basic_call"], |c| {
            c.cps = 1.0;
            c.duration = Duration::from_secs(1);
        })
        .await;
        assert_eq!(records.len(), 1, "{records:?}");
        let r = &records[0];
        assert_common(r);
        let ending = (r["ending"].as_str(), r["producer_verdict"].as_str());
        assert_eq!(ending, (Some("failed"), Some("transport")), "{r}");
        let kinds: Vec<&str> =
            r["failures"].as_array().unwrap().iter().filter_map(|f| f["kind"].as_str()).collect();
        assert_eq!(kinds, ["transport"], "{r}");
        if answered {
            let fin = (r["final_code"].as_u64(), r["reason"].clone());
            assert_eq!(fin, (Some(200), Value::Null), "{r}");
        } else {
            let fin = (r["final_code"].clone(), r["reason"].as_str());
            assert_eq!(fin, (Value::Null, Some("transport")), "{r}");
        }
    }
}
