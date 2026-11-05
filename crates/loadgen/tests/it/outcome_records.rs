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

use crate::fake_rig::{addr, cfg, mix, setup_fake, setup_fake_faulty, setup_fake_tuned, RECV};

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
/// `abandoned` a CANCELing shape declares, never a 487. The SUT's
/// first-response deadline sits past the caller's wait, so the caller gives up
/// first.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_stalled_setup_is_a_timeout_whatever_its_teardown_draws() {
    let rig = setup_fake_tuned(7810, |c| {
        c.invite_first_response_timeout_sec =
            b2bua_sdk::B2buaConfig::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC;
    })
    .await;
    let records = run(rig, &["basic_call", "abandon_ringing"], |c| {
        c.default_tuning = CallTuning {
            drop_nth: Some(
                TargetedDrop::request("INVITE", 1, DropDir::Inbound).permanent().on_leg("bob"),
            ),
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

/// The records of one call against a raw peer that answers an INVITE 100 Trying
/// and nothing else, with `final_wait` on the call.
async fn against_a_peer_silent_after_its_100(
    base: u16,
    final_wait: Option<Duration>,
) -> Vec<Value> {
    use loadgen::{Correlation, EndpointSpec, Role};
    use sip_net::{BindUdpOpts, SignalingNetwork};

    let sim = Arc::new(sip_net_testkit::SimulatedSignalingNetwork::new(1));
    let clock = sip_clock::Clock::test_at(0);
    let correlation = Correlation::header("X-Loadgen-Id");
    let (uac, uas, refer, peer) = (addr(base), addr(base + 1), addr(base + 2), addr(base + 3));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![
            EndpointSpec { addr: uac, role: Role::Caller },
            EndpointSpec { addr: uas, role: Role::Callee },
            EndpointSpec { addr: refer, role: Role::Callee },
        ],
        correlation.clone(),
        64,
        8,
        RECV,
        clock.clone(),
    )
    .await
    .unwrap();
    let socket = sim.bind_udp(BindUdpOpts::new(peer, 64)).await.unwrap();
    let silent = tokio::spawn(async move {
        while let Some(p) = socket.recv().await {
            let text = String::from_utf8_lossy(&p.raw).into_owned();
            if !text.starts_with("INVITE ") {
                continue;
            }
            let copied: Vec<&str> = text
                .lines()
                .filter(|l| {
                    ["Via:", "From:", "To:", "Call-ID:", "CSeq:"].iter().any(|h| l.starts_with(h))
                })
                .collect();
            let trying = format!(
                "SIP/2.0 100 Trying\r\n{}\r\nContent-Length: 0\r\n\r\n",
                copied.join("\r\n")
            );
            let _ = socket.send_to(trying.as_bytes(), p.src).await;
        }
    });
    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: uac,
        uas_addr: uas,
        refer_addr: refer,
        correlation,
        recv_timeout: RECV,
        clock,
    });
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let file =
        std::env::temp_dir().join(format!("loadgen-silent-{}-{base}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let sink = Arc::new(OutcomeSink::new("run-1", "loadgen", "main", &file, None).unwrap());
    let mut c = cfg(peer, 1.0, 1, 4, 0x51);
    c.call.final_wait = final_wait;
    Driver::new(c, vec![mix("basic_call", 1.0)], reporter, transport)
        .with_outcomes(sink.clone())
        .run()
        .await;
    sink.close();
    silent.abort();
    let records = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON record per line"))
        .collect();
    let _ = std::fs::remove_file(&file);
    records
}

/// How long the record's caller waited for its final, and its 100's instant.
fn waited_for_its_final(r: &Value) -> i64 {
    assert_common(r);
    assert_eq!((r["ending"].as_str(), r["final_code"].as_u64()), (Some("timeout"), None), "{r}");
    let started = r["started_at_ms"].as_i64().unwrap();
    let trying = r["trying_at_ms"].as_i64().expect("the caller heard the 100");
    assert!(trying - started < 1_000, "{r}");
    r["setup_ended_at_ms"].as_i64().unwrap() - started
}

/// A caller who heard a 100 and no final waits the final wait, far past the
/// standard 32 s step, and records both the 100 and the instant it gave up.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_100_and_no_final_waits_the_final_wait() {
    let records = against_a_peer_silent_after_its_100(7830, Some(Duration::from_secs(250))).await;
    assert!(!records.is_empty());
    for r in &records {
        let waited = waited_for_its_final(r);
        assert!((250_000..255_000).contains(&waited), "gave up at the final wait: {r}");
    }
}

/// With no final wait the caller gives up at the standard 32 s step.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_100_and_no_final_gives_up_at_the_standard_step_with_no_final_wait() {
    let records = against_a_peer_silent_after_its_100(7840, None).await;
    assert!(!records.is_empty());
    for r in &records {
        let waited = waited_for_its_final(r);
        assert!((32_000..37_000).contains(&waited), "gave up at the standard step: {r}");
    }
}

/// The caller's BYE never reaches the SUT and draws no final: `stranded`,
/// the dialog never released.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_without_a_final_is_stranded() {
    let records = run(setup_fake(7820).await, &["basic_call"], |c| {
        c.default_tuning = CallTuning {
            drop_nth: Some(TargetedDrop::request("BYE", 1, DropDir::Outbound).permanent()),
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
        let fault: sip_net_testkit::SendFault = Arc::new(move |src, _dst| {
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
