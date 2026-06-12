//! Phase J + K acceptance (ADR-0018): the media-on basic call produces
//! classified, playable `.wav`s on BOTH infra shapes ("alice hears bob" as a
//! check); the `rerouting` / `rerouting-prack` shapes run green over the fake
//! failover-capable SUT; and the committed `full` campaign — every case ×
//! the shared `invite-identity` check set — runs green through the executor.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use e2e_core::checks::{self, Bindings};
use e2e_core::model;
use e2e_core::{
    BasicCallMedia, CallflowShape, EndpointConfig, FakeLsbcB2bua, InfraShape, Input,
    RealLoopbackDirect, Rerouting, ReroutingPrack, run,
};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn fake_cfg() -> EndpointConfig {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("bob2", "127.0.0.1:5071"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
    }
}

fn assert_media_captures(captures: &[e2e_core::media::MediaCapture], dir_tag: &str) {
    let dir = std::env::temp_dir().join(format!("e2e-media-{dir_tag}-{}", std::process::id()));
    let (refs, verdicts) = e2e_core::media::write_and_fold(&dir, captures).unwrap();
    assert_eq!(verdicts.len(), 2);
    for v in &verdicts {
        assert!(v.passed, "{}.{}: {} (actual {:?})", v.on, v.field, v.detail, v.actual);
    }
    let hears = |agent: &str| {
        verdicts.iter().find(|v| v.on == format!("{agent}.media")).unwrap().actual.clone()
    };
    assert_eq!(hears("alice").as_deref(), Some("Bob"), "alice hears bob");
    assert_eq!(hears("bob1").as_deref(), Some("Alice"), "bob hears alice");
    // The artifacts are real RIFF/WAVE files with audio in them.
    for r in &refs {
        let bytes = std::fs::read(dir.join(&r.wav)).unwrap();
        assert!(bytes.len() > 8_000, "{} carries audio ({} bytes)", r.wav, bytes.len());
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert!(r.rms > 0.01, "{} not silence (rms {})", r.wav, r.rms);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Phase J over the FAKE infra: RTP rides the simulated fabric under the
/// paused clock; the talk window auto-advances.
#[tokio::test(start_paused = true)]
async fn media_call_over_fake_infra_classifies_both_ways() {
    let mut rt = FakeLsbcB2bua.build("media/fake", &fake_cfg()).await;
    BasicCallMedia.run(&mut rt, &Input::default()).await;
    let captures = rt.take_media();
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());
    assert_media_captures(&captures, "fake");
}

/// Phase J over the REAL infra: real loopback UDP for both SIP and RTP, wall
/// clock, the same shape body.
#[tokio::test]
async fn media_call_over_real_infra_classifies_both_ways() {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:38060"),
        ("bob1", "127.0.0.1:38070"),
        ("alice.rtp", "127.0.0.1:48000"),
        ("bob1.rtp", "127.0.0.1:48002"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    let cfg = EndpointConfig {
        schema: None,
        infra_shape: "real-loopback-direct".into(),
        roles,
        recv_timeout_ms: 5_000,
        transit_delay_ms: 0,
    };
    let mut rt = RealLoopbackDirect.build("media/real", &cfg).await;
    BasicCallMedia.run(&mut rt, &Input::default()).await;
    let captures = rt.take_media();
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());
    assert_media_captures(&captures, "real");
}

/// Phase K: bob1 rejects, the SUT re-targets bob2, identity survives to the
/// rerouted leg — verified via the committed case's own checks.
#[tokio::test(start_paused = true)]
async fn rerouting_shape_fails_over_to_bob2() {
    let case =
        model::load_test_case(&workspace_root().join("e2e/cases/rerouting-identity.json")).unwrap();
    let check_sets = model::load_check_sets(&workspace_root().join("e2e/checksets")).unwrap();
    model::validate_case(&case, &e2e_core::shapes::registry(), &check_sets).unwrap();

    let mut rt = FakeLsbcB2bua.build("rerouting/fake", &fake_cfg()).await;
    let lb_vip = rt.lb_vip;
    Rerouting.run(&mut rt, &case.input.core).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());

    let verdicts = checks::evaluate_case(
        &case,
        &check_sets,
        &report,
        &Bindings { input: &case.input, lb_vip },
    );
    assert!(!verdicts.is_empty());
    for v in &verdicts {
        assert!(v.passed, "{}.{}: {} (actual {:?})", v.on, v.field, v.detail, v.actual);
    }
}

/// Phase K: rerouting + reliable 183/PRACK on the winning leg.
#[tokio::test(start_paused = true)]
async fn rerouting_prack_shape_relays_reliable_provisional() {
    let case = model::load_test_case(
        &workspace_root().join("e2e/cases/rerouting-prack-identity.json"),
    )
    .unwrap();
    let check_sets = model::load_check_sets(&workspace_root().join("e2e/checksets")).unwrap();
    model::validate_case(&case, &e2e_core::shapes::registry(), &check_sets).unwrap();

    let mut rt = FakeLsbcB2bua.build("rerouting-prack/fake", &fake_cfg()).await;
    let lb_vip = rt.lb_vip;
    ReroutingPrack.run(&mut rt, &case.input.core).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());

    let verdicts = checks::evaluate_case(
        &case,
        &check_sets,
        &report,
        &Bindings { input: &case.input, lb_vip },
    );
    for v in &verdicts {
        assert!(v.passed, "{}.{}: {} (actual {:?})", v.on, v.field, v.detail, v.actual);
    }
}

/// The committed `full` campaign — all four cases (identity, media, rerouting,
/// rerouting-prack) sharing the invite-identity set — runs green through the
/// executor, with the media cell's `.wav`s landing beside its result.json.
#[test]
fn committed_full_campaign_runs_green() {
    let e2e = workspace_root().join("e2e");
    let runs_root = std::env::temp_dir().join(format!("e2e-full-{}", std::process::id()));
    let spec =
        run::load_spec(&e2e, &e2e.join("campaigns/full.json"), runs_root.clone(), "t0".into())
            .expect("committed full campaign loads");
    let result = run::run_blocking(&spec).expect("full campaign runs");
    assert!(result.passed(), "{:#?}", result.index);
    assert_eq!(result.index.cells.len(), 4);

    // The media cell persisted its artifacts + refs + hears-verdicts.
    let media_cell = result
        .index
        .cells
        .iter()
        .find(|c| c.cell.case == "basic-call-media")
        .unwrap();
    let cell_dir = result.run_dir.join(&media_cell.dir);
    let media_result = e2e_core::result::read_result(&cell_dir).unwrap();
    assert_eq!(media_result.media.len(), 2);
    for m in &media_result.media {
        assert!(cell_dir.join(&m.wav).is_file(), "{} written", m.wav);
    }
    assert!(
        media_result.checks.iter().any(|v| v.on == "alice.media" && v.passed),
        "the hears-check is folded into the cell's checks"
    );

    std::fs::remove_dir_all(&runs_root).ok();
}
