use super::*;

fn rfc_fail(rule: &str, detail: &str) -> CallOutcome {
    CallOutcome::RfcAuditFail(vec![sip_net_testkit::RfcFinding {
        rule: rule.to_string(),
        lane: "10.0.0.1:5060".to_string(),
        detail: detail.to_string(),
        advisory: false,
        offending: None,
        charged: None,
    }])
}

/// The latency histograms are exported as Prometheus histograms, so a
/// scrape series gives the latency of any time window (histogram_quantile
/// over an increase).
#[test]
#[ignore = "slow lane: loadgen"]
fn latency_histograms_are_exported_as_prometheus_histograms() {
    let r = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
    for (setup, e2e) in [(5, 9_000), (50, 9_500), (900_000, 950_000)] {
        r.record(
            "basic_call",
            &CallOutcome::Ok { branch: "answered" },
            "",
            Duration::from_millis(e2e),
            &[("time_to_200", Duration::from_millis(setup))],
            None,
        );
    }
    let prom = r.render_prometheus();
    assert!(prom.contains("# TYPE loadgen_checkpoint_latency_seconds histogram"), "{prom}");
    assert!(prom.contains("# TYPE loadgen_e2e_latency_seconds histogram"), "{prom}");
    let family = "loadgen_checkpoint_latency_seconds";
    let labels = "stream=\"main\",shape=\"basic_call\",checkpoint=\"time_to_200\"";
    let buckets: Vec<(f64, u64)> = prom
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{family}_bucket{{{labels},le=\"")))
        .map(|rest| {
            let (le, n) = rest.split_once("\"} ").unwrap();
            let le = if le == "+Inf" { f64::INFINITY } else { le.parse().unwrap() };
            (le, n.parse().unwrap())
        })
        .collect();
    assert_eq!(buckets.len(), 49, "48 bounds and +Inf: {prom}");
    assert!(buckets.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1), "{buckets:?}");
    let at = |secs: f64| buckets.iter().find(|(le, _)| *le >= secs).unwrap().1;
    assert_eq!(at(0.005), 1, "5 ms falls in the first bucket reaching 5 ms");
    assert_eq!(at(0.05), 2);
    assert_eq!(buckets[47].1, 2, "900 s is past the last bound (~730 s)");
    assert_eq!(buckets[48], (f64::INFINITY, 3));
    assert!(prom.contains(&format!("{family}_count{{{labels}}} 3\n")), "{prom}");
    assert!(prom.contains(&format!("{family}_sum{{{labels}}} 900.055\n")), "{prom}");
    assert!(
        prom.contains(
            "loadgen_e2e_latency_seconds_count{stream=\"main\",shape=\"basic_call\"} 3\n"
        ),
        "{prom}"
    );
}

/// A scrape writes no lifetime quantile gauge (a window's latency is a
/// `histogram_quantile` over the histograms); it counts each ok call under
/// its accepted branch and each failed call under its failed step.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_scrape_holds_no_quantile_gauge_and_counts_branches_and_steps() {
    let r = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
    r.declare_scenarios(["basic_call"]);
    r.record(
        "basic_call",
        &CallOutcome::Ok { branch: "answered" },
        "",
        Duration::from_millis(9_000),
        &[("time_to_200", Duration::from_millis(5))],
        None,
    );
    let failed = CallOutcome::Step {
        at: FailedAt { who: "alice", goal: Some(("bye", 1)), barrier: "established" },
        cause: scenario_harness::StepError::timeout("alice", None, "a datagram"),
    };
    r.record("basic_call", &failed, &failed.case(None), Duration::from_millis(9_000), &[], None);
    let prom = r.render_prometheus();
    assert!(!prom.contains("loadgen_e2e_seconds"), "{prom}");
    assert!(!prom.contains("loadgen_checkpoint_seconds"), "{prom}");
    assert!(!prom.contains("quantile=\""), "{prom}");
    assert!(!prom.contains("expected_reject"), "{prom}");
    assert!(
        prom.contains(
            "loadgen_branch_total{stream=\"main\",shape=\"basic_call\",branch=\"answered\"} 1\n"
        ),
        "{prom}"
    );
    assert!(prom.contains(
        "loadgen_failed_step_total{stream=\"main\",shape=\"basic_call\",step=\"alice:bye#1@established\"} 1\n"
    ));
}

/// A SIP round trip renders on `loadgen_rtt_seconds{stream,shape,exchange}`,
/// bounds from 0.1 ms in ×1.25 steps: a 2.5 ms and a 3.5 ms exchange land
/// in different buckets, and only observed exchanges are written.
#[test]
#[ignore = "slow lane: loadgen"]
fn round_trips_render_as_a_fine_histogram_per_exchange() {
    let r = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
    r.declare_scenarios(["basic_call"]);
    r.record_rtts(
        "basic_call",
        &[
            (Exchange::Invite18x, Duration::from_micros(2_500)),
            (Exchange::Invite18x, Duration::from_micros(3_500)),
            (Exchange::ByeFinal, Duration::from_millis(40)),
        ],
    );
    let prom = r.render_prometheus();
    assert!(prom.contains("# TYPE loadgen_rtt_seconds histogram"), "{prom}");
    let labels = "stream=\"main\",shape=\"basic_call\",exchange=\"invite_18x\"";
    let buckets: Vec<(f64, u64)> = prom
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("loadgen_rtt_seconds_bucket{{{labels},le=\"")))
        .map(|rest| {
            let (le, n) = rest.split_once("\"} ").unwrap();
            let le = if le == "+Inf" { f64::INFINITY } else { le.parse().unwrap() };
            (le, n.parse().unwrap())
        })
        .collect();
    assert_eq!(buckets.len(), 61, "60 bounds and +Inf: {prom}");
    assert_eq!(buckets[0].0, 0.0001, "the first bound is 0.1 ms");
    assert!(buckets[59].0 > 50.0, "the last bound reaches past 50 s: {buckets:?}");
    assert!(
        buckets[..60].windows(2).all(|w| w[1].0 / w[0].0 < 1.26),
        "bounds at most 25 % apart: {buckets:?}"
    );
    let at = |secs: f64| buckets.iter().find(|(le, _)| *le >= secs).unwrap().1;
    assert_eq!(at(0.0025), 1, "2.5 ms alone at its bucket");
    assert_eq!(at(0.0035), 2);
    assert!(prom.contains(&format!("loadgen_rtt_seconds_count{{{labels}}} 2\n")), "{prom}");
    assert!(prom.contains(&format!("loadgen_rtt_seconds_sum{{{labels}}} 0.006\n")), "{prom}");
    assert!(
        prom.contains(
            "loadgen_rtt_seconds_count{stream=\"main\",shape=\"basic_call\",exchange=\"bye_final\"} 1\n"
        ),
        "{prom}"
    );
    assert!(!prom.contains("exchange=\"invite_100\""), "an unobserved exchange is not written");
}

/// A bucket under cap keeps its scenario recording for [`CHASE_CALLS`]
/// calls after its last sample, then only the background fraction does;
/// its next sample re-arms the chase.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_bucket_under_cap_chases_its_next_sample_for_a_bounded_run() {
    let r = Reporter::new(ReporterCfg { sample_cap: 2, background_record_every: 0 });
    assert!(r.should_record("s"), "one of the first sample_cap calls");
    let failed = || CallOutcome::Step {
        at: FailedAt::at_barrier("established"),
        cause: scenario_harness::StepError::timeout("established", None, "a datagram"),
    };
    let record = |o: &CallOutcome| {
        let sample = RenderedSample { html: None, detail: o.detail(), e2e_ms: 1.0 };
        r.record("s", o, &o.case(None), Duration::ZERO, &[], Some(sample));
    };
    record(&failed());
    let chased = (0..CHASE_CALLS + 10).filter(|_| r.should_record("s")).count();
    assert_eq!(chased, CHASE_CALLS as usize, "the chase is bounded");
    record(&failed());
    assert!(!r.should_record("s"), "a full bucket is not chased");
    record(&CallOutcome::Step {
        at: FailedAt::at_barrier("torn_down"),
        cause: scenario_harness::StepError::timeout("torn_down", None, "a datagram"),
    });
    assert!(r.should_record("s"), "a new bucket re-arms the chase");
}

/// Before any call of a scenario has ended, the gate records the first
/// `sample_cap` calls of the scenario and the background share, whatever the
/// number of calls placed: long holds never record a whole hold time of calls.
#[test]
#[ignore = "slow lane: loadgen"]
fn before_any_outcome_the_gate_records_the_first_samples_and_the_share() {
    let (sample_cap, every, placed) = (3u32, 16u64, 1_000u64);
    let r = Reporter::new(ReporterCfg { sample_cap, background_record_every: every });
    let recorded = (0..placed).filter(|_| r.should_record("s")).count() as u64;
    let bound = u64::from(sample_cap) + placed.div_ceil(every);
    assert!(recorded <= bound, "{recorded} of {placed} calls recorded, bound {bound}");
    assert!(recorded >= placed.div_ceil(every), "the background share records");

    let first = Reporter::new(ReporterCfg { sample_cap, background_record_every: 0 });
    let firsts: Vec<bool> = (0..6).map(|_| first.should_record("s")).collect();
    assert_eq!(firsts, [true, true, true, false, false, false], "the first sample_cap calls");

    let never = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
    assert!((0..100).all(|_| !never.should_record("s")), "no cap and no share records nothing");
}

/// The index counts an accepted reject as ok: it is no failed call.
#[test]
#[ignore = "slow lane: loadgen"]
fn the_index_counts_an_accepted_reject_as_ok() {
    let r = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
    let reject = CallOutcome::Ok { branch: "rejected" };
    r.record("limited", &reject, &reject.case(None), Duration::from_millis(3), &[], None);
    let idx = r.build_index(
        LoadRunMeta {
            started_ms: 0,
            finished_ms: 0,
            finished: true,
            target: "t".into(),
            cps: 1.0,
            duration_secs: 1,
            max_in_flight: 1,
            egress: None,
            profile: None,
        },
        Canaries::default(),
    );
    let row = idx.counts.iter().find(|c| c.class == "ok").expect("a row");
    assert!(row.ok, "{row:?}");
    assert_eq!((idx.total_calls(), idx.failed_calls()), (1, 0));
}

/// The case dimension splits ONE class into per-failure-mode sample buckets:
/// two different RFC rules on the same scenario each keep their own first-N
/// samples (N samples of the first rule to fire never starve a second,
/// rarer rule), and the counts table carries the case.
#[test]
#[ignore = "slow lane: loadgen"]
fn case_splits_sample_buckets_within_a_class() {
    let r = Reporter::new(ReporterCfg { sample_cap: 1, background_record_every: 0 });
    let mk =
        |d: &str| Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 1.0 });
    let unacked = rfc_fail("unacked-invite-non-2xx-final", "reject never ACKed");
    let cseq = rfc_fail("rfc3261.cseq", "CSeq went backwards");
    let (case_a, case_b) = (unacked.case(None), cseq.case(None));
    assert_ne!(case_a, case_b, "different rules → different cases");

    // Fill rule A's bucket (cap 1), then rule B must STILL want a sample.
    r.record("reinvite", &unacked, &case_a, Duration::from_millis(1), &[], mk("a"));
    assert!(
        !r.wants_sample("reinvite", &ResultClass::RfcAuditFail, &case_a),
        "rule A bucket full at cap"
    );
    assert!(
        r.wants_sample("reinvite", &ResultClass::RfcAuditFail, &case_b),
        "rule B keeps its own bucket"
    );
    r.record("reinvite", &cseq, &case_b, Duration::from_millis(1), &[], mk("b"));
    assert_eq!(r.sample_count("reinvite", &ResultClass::RfcAuditFail), 2);

    // The machine-readable index carries the case on counts AND samples,
    // and the per-case pages land in per-case dirs.
    let out = std::env::temp_dir().join(format!("loadgen-case-split-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    r.finalize(&out).unwrap();
    let idx = r.build_index(
        LoadRunMeta {
            started_ms: 0,
            finished_ms: 0,
            finished: true,
            target: "t".into(),
            cps: 1.0,
            duration_secs: 1,
            max_in_flight: 1,
            egress: None,
            profile: None,
        },
        Canaries::default(),
    );
    assert!(idx.counts.iter().any(|c| c.case == case_a && c.count == 1));
    assert!(idx.counts.iter().any(|c| c.case == case_b && c.count == 1));
    for group in &idx.samples {
        for page in &group.pages {
            assert!(out.join(page).exists(), "index names a missing page: {page}");
            assert!(page.contains(&group.case), "page path carries the case: {page}");
        }
    }
    let _ = std::fs::remove_dir_all(&out);
}

/// `finalize_run` writes `load-result.json` next to `index.html`; it parses
/// back as a `LoadRunIndex`, its counts/canaries/check-verdicts mirror the
/// reporter state, and every sample page it names actually exists on disk (the
/// index and the HTML report never disagree).
#[test]
#[ignore = "slow lane: loadgen"]
fn writes_and_parses_the_machine_readable_index() {
    let r = Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 0 });
    let mk =
        |d: &str| Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 2.0 });

    // One OK basic call, one genuine check-fail reinvite (sampled), plus the
    // cross-call ringing gate + a sampled check tally.
    r.record(
        "basic_call",
        &CallOutcome::Ok { branch: "answered" },
        "",
        Duration::from_millis(3),
        &[("ringing", Duration::from_millis(1))],
        mk("ok"),
    );
    let check_fail = CallOutcome::CheckFail(vec![e2e_model::CheckVerdict {
        on: "alice.invite".to_string(),
        field: "from.userInfo".to_string(),
        op: e2e_model::CheckOp::Exists,
        expected: None,
        actual: None,
        passed: false,
        detail: "from.userInfo mismatch".to_string(),
    }]);
    let check_case = check_fail.case(None);
    r.record("reinvite", &check_fail, &check_case, Duration::from_millis(5), &[], mk("check fail"));
    r.record_ringing(Some(true));
    r.record_ringing(Some(false));
    r.record_checks("reinvite", true);
    r.record_checks("reinvite", false);
    r.inc_shed("basic_call");

    let out = std::env::temp_dir().join(format!("loadgen-index-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let meta = LoadRunMeta {
        started_ms: 1_000,
        finished_ms: 61_000,
        finished: true,
        target: "10.0.0.1:5060".to_string(),
        cps: 20.0,
        duration_secs: 60,
        max_in_flight: 100,
        egress: Some("transparent".to_string()),
        profile: Some("smoke".to_string()),
    };
    let canaries = Canaries { orphans: 0, drops: 7, ..Canaries::default() };
    r.finalize_run(&out, meta.clone(), canaries).unwrap();

    let path = out.join("load-result.json");
    assert!(path.exists(), "load-result.json missing next to index.html");
    assert!(out.join("index.html").exists(), "index.html still written");
    let idx: LoadRunIndex = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

    assert_eq!(idx.meta, meta, "run metadata echoed verbatim");
    assert_eq!(idx.total_calls(), 2);
    assert_eq!(idx.failed_calls(), 1, "the check_fail reinvite");
    assert!(idx.counts.iter().any(|c| c.scenario == "basic_call" && c.class == "ok" && c.ok));
    assert!(idx
        .counts
        .iter()
        .any(|c| c.scenario == "reinvite" && c.class == "check_fail" && !c.ok));

    // Canaries: reporter-owned shed/ringing filled in; caller-owned drops kept.
    assert_eq!(idx.canaries.shed, 1);
    assert_eq!(idx.canaries.drops, 7);
    assert_eq!((idx.canaries.ringing_received, idx.canaries.ringing_expected), (1, 2));

    // Per-scenario check-verdict tally.
    let cs = idx.checks.iter().find(|c| c.scenario == "reinvite").expect("reinvite check row");
    assert_eq!((cs.passed, cs.failed), (1, 1));

    // Every listed sample page exists on disk (the paths match what finalize wrote).
    assert!(!idx.samples.is_empty(), "at least one sample group");
    for group in &idx.samples {
        for page in &group.pages {
            assert!(out.join(page).exists(), "index names a missing sample page: {page}");
        }
    }
    let _ = std::fs::remove_dir_all(&out);
}

/// The periodic snapshot task races the final `finished:true` write whenever
/// the report interval divides the run duration — a tick fired at the run's
/// last instant would overwrite the final index with a `finished:false`
/// snapshot. The shutdown sequence must abort+await the task BEFORE the
/// final write; after that, no tick may ever land again.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn final_write_is_not_overwritten_by_snapshot_task() {
    let r = Arc::new(Reporter::new(ReporterCfg { sample_cap: 1, background_record_every: 0 }));
    r.record(
        "basic_call",
        &CallOutcome::Ok { branch: "answered" },
        "",
        Duration::from_millis(3),
        &[],
        None,
    );

    let out = std::env::temp_dir().join(format!("loadgen-snapshot-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let base = LoadRunMeta {
        started_ms: 0,
        finished_ms: 0,
        finished: false,
        target: "10.0.0.1:5060".to_string(),
        cps: 1.0,
        duration_secs: 60,
        max_in_flight: 1,
        egress: None,
        profile: None,
    };
    let read_finished = |out: &Path| -> bool {
        let idx: LoadRunIndex =
            serde_json::from_str(&std::fs::read_to_string(out.join("load-result.json")).unwrap())
                .unwrap();
        idx.meta.finished
    };

    let snap_meta = base.clone();
    let snap = r.spawn_snapshots(
        out.clone(),
        Duration::from_secs(1),
        move || snap_meta.clone(),
        Canaries::default,
    );

    // Let a few ticks land: mid-run snapshots say finished:false.
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert!(!read_finished(&out), "mid-run snapshots are finished:false");

    // The shutdown sequence under test: stop the task, THEN write the final.
    snap.abort();
    let _ = snap.await;
    r.finalize_run(&out, LoadRunMeta { finished: true, ..base }, Canaries::default()).unwrap();
    assert!(read_finished(&out), "final write says finished:true");

    // Advance past several more would-be ticks — a leaked
    // task would overwrite the final index with a finished:false snapshot.
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert!(read_finished(&out), "no snapshot tick may land after shutdown");
    let _ = std::fs::remove_dir_all(&out);
}

/// The first unrecorded occurrence of a bucket arms its chase; later
/// unrecorded occurrences never re-arm it, a recorded sample does.
#[test]
#[ignore = "slow lane: loadgen"]
fn an_unrecorded_occurrence_arms_the_chase_once() {
    let r = Reporter::new(ReporterCfg { sample_cap: 2, background_record_every: 0 });
    assert!(r.should_record("s") && r.should_record("s"), "the first sample_cap calls");
    let failed = CallOutcome::Step {
        at: FailedAt::at_barrier("established"),
        cause: scenario_harness::StepError::timeout("established", None, "a datagram"),
    };
    let case = failed.case(None);
    r.record("s", &failed, &case, Duration::ZERO, &[], None);
    let chased = (0..CHASE_CALLS + 10).filter(|_| r.should_record("s")).count();
    assert_eq!(chased, CHASE_CALLS as usize, "an unrecorded occurrence arms a bounded chase");
    r.record("s", &failed, &case, Duration::ZERO, &[], None);
    assert!(!r.should_record("s"), "a later unrecorded occurrence does not re-arm it");
    let flow = RenderedSample { html: Some(Arc::from("<html/>")), detail: None, e2e_ms: 1.0 };
    r.record("s", &failed, &case, Duration::ZERO, &[], Some(flow));
    assert!(r.should_record("s"), "a recorded sample under cap re-arms it");
}

/// A bucket counted with no stored sample says so in the index.
#[test]
#[ignore = "slow lane: loadgen"]
fn the_index_names_a_bucket_with_no_recorded_call() {
    let r = Reporter::new(ReporterCfg { sample_cap: 2, background_record_every: 64 });
    let failed = rfc_fail("rule", "detail");
    r.record("s", &failed, &failed.case(None), Duration::ZERO, &[], None);
    let idx = r.render_report().index;
    assert!(idx.contains("none recorded (1 in 64 calls is recorded)"), "{idx}");
}
