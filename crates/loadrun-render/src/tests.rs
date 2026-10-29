use e2e_model::{
    Canaries, CountRow, FailureRow, JudgedBasis, JudgedShare, LatencyRow, LoadRunIndex,
    LoadRunMeta, Progress, RateEvent, RateEventKind, SampleGroup, Sampling, ScenarioRecording,
    SeriesPoint,
};

use super::*;

fn window(start_s: u64, span_s: u64, ok: u64, failed: u64) -> SeriesPoint {
    SeriesPoint {
        start_ms: start_s * 1000,
        span_ms: span_s * 1000,
        placed: ok + failed,
        ok,
        failed,
        rejected: 0,
        p50_ms: 12.0,
        p99_ms: 80.0,
    }
}

fn index(finished: bool) -> LoadRunIndex {
    let long_detail = format!("alice expected 200, got 486 <Busy> {}", "x".repeat(300));
    LoadRunIndex {
        meta: LoadRunMeta {
            started_ms: 1_700_000_000_000,
            finished_ms: 1_700_000_035_000,
            finished,
            target: "192.0.2.10:5060".into(),
            cps: 20.0,
            duration_secs: 60,
            max_in_flight: 500,
            egress: Some("transparent".into()),
            profile: None,
            command_line: vec![
                "loadgen".into(),
                "--cps".into(),
                "20".into(),
                "--case".into(),
                "a b".into(),
            ],
            sampling: Some(Sampling { sample_cap: 10, record_every: 64, chase_calls: 64 }),
        },
        progress: Progress {
            elapsed_ms: 35_000,
            target_cps: 10.0,
            draining: false,
            offered: 700,
            placed: 693,
            in_flight: 41,
            ok: 640,
            failed: 9,
            rejected: 3,
            not_placed: 7,
            generator_failures: 0,
        },
        // 25 windows, the oldest two merged to 10 s; a rate event in the
        // window 30–35 s... and one in the oldest window.
        series: std::iter::once(window(0, 10, 50, 0))
            .chain((0..24).map(|i| window(10 + i * 5, 5, 25 + i, i % 3)))
            .collect(),
        rate_events: vec![
            RateEvent { at_ms: 4_000, kind: RateEventKind::Rate, from_cps: 20.0, to_cps: 15.0 },
            RateEvent { at_ms: 112_500, kind: RateEventKind::Rate, from_cps: 15.0, to_cps: 10.0 },
        ],
        rate_events_dropped: 3,
        recent_failures: vec![
            FailureRow {
                at_ms: 20_000,
                scenario: "basic_call".into(),
                class: "timeout".into(),
                case: "bob@ringing".into(),
                page: None,
            },
            FailureRow {
                at_ms: 31_000,
                scenario: "basic_call".into(),
                class: "status_486".into(),
                case: "alice@answered".into(),
                page: Some("callflows/basic_call/status_486/alice@answered/0.html".into()),
            },
        ],
        recording: vec![ScenarioRecording {
            scenario: "basic_call".into(),
            placed: 693,
            recorded: 40,
            accepted: 640,
            audited: 30,
        }],
        counts: vec![
            CountRow {
                scenario: "basic_call".into(),
                class: "ok".into(),
                case: String::new(),
                count: 640,
                ok: true,
                judged: None,
                error: None,
            },
            CountRow {
                scenario: "basic_call".into(),
                class: "rfc_audit_fail".into(),
                case: "rule-x".into(),
                count: 2,
                ok: false,
                judged: Some(JudgedShare {
                    failed: 2,
                    judged: 30,
                    base: 640,
                    basis: JudgedBasis::Audited,
                }),
                error: None,
            },
            CountRow {
                scenario: "basic_call".into(),
                class: "status_486".into(),
                case: "alice@answered".into(),
                count: 7,
                ok: false,
                judged: None,
                error: Some(long_detail),
            },
        ],
        latency: vec![LatencyRow {
            scenario: "basic_call".into(),
            n: 649,
            mean_ms: 20.0,
            p50_ms: 12.0,
            p90_ms: 40.0,
            p99_ms: 80.0,
            max_ms: 120.0,
        }],
        checkpoints: vec![],
        rtt: vec![],
        checks: vec![],
        canaries: Canaries {
            orphans: 2,
            shed: 4,
            drops_in: 11,
            drops_out: 13,
            inbox_drops: 0,
            ringing_expected: 100,
            ringing_received: 97,
        },
        samples: vec![SampleGroup {
            scenario: "basic_call".into(),
            class: "status_486".into(),
            case: "alice@answered".into(),
            pages: vec!["callflows/basic_call/status_486/alice@answered/0.html".into()],
        }],
    }
}

fn live() -> RenderOpts {
    RenderOpts { sample_prefix: String::new(), live: Some(Live { refresh_secs: 60 }) }
}

#[test]
fn the_command_line_comes_first_quoted_as_typed() {
    let page = render_body(&index(false), &RenderOpts::default());
    let cmd = page.find("<pre class=\"cmd\">loadgen --cps 20 --case &#39;a b&#39;</pre>");
    assert_eq!(cmd, Some(0), "the command line opens the page: {page}");
    assert!(page.contains("target <code>192.0.2.10:5060</code>"));
}

#[test]
fn the_headline_states_rate_flight_and_every_rejection_source() {
    let page = render_body(&index(false), &RenderOpts::default());
    for want in [
        "0:35 / 1:00 (running)",
        "target 10 cps (configured 20) · achieved 19.8 cps",
        "41 / 500 max",
        "offered 700 · placed 693",
        "ok 640",
        "failed 9",
        "rejected class 3",
        "shed at max-in-flight 4",
        "not placed 7",
        "generator failures 0",
        "in 11 · out 13",
        "orphans 2",
        "inbox drops 0",
        "0.9700",
    ] {
        assert!(page.contains(want), "headline lacks {want:?}: {page}");
    }
}

#[test]
fn a_live_page_carries_the_controls_and_reloads() {
    let idx = index(false);
    let page = render_page(&idx, &live());
    assert!(page.contains("<meta http-equiv=\"refresh\" content=\"60\">"));
    assert!(page.contains("action=\"/rate\"") && page.contains("name=\"cps\""));
    assert!(page.contains("action=\"/drain\""));
    assert!(page.contains("href=\"?refresh=0\""), "a freeze link");

    let frozen = render_page(&idx, &RenderOpts { live: Some(Live { refresh_secs: 0 }), ..live() });
    assert!(!frozen.contains("http-equiv=\"refresh\""), "a frozen page does not reload");
    assert!(frozen.contains("href=\"?refresh=60\""), "a link to reload again");
    assert!(frozen.contains("action=\"/rate\""), "frozen keeps the controls");
}

#[test]
fn a_finished_page_has_no_controls_and_keeps_everything_else() {
    let finished = index(true);
    let page = render_page(&finished, &live());
    assert!(!page.contains("http-equiv=\"refresh\""));
    assert!(!page.contains("<form"));
    assert!(page.contains("(finished)"));
    let live_body = render_body(&index(false), &live());
    for section in [
        "Headline",
        "Time series",
        "Last failures",
        "Results by",
        "Latency",
        "Rate events",
        "<h2>Run</h2>",
    ] {
        assert!(page.contains(section) && live_body.contains(section), "{section} on both pages");
    }
}

#[test]
fn a_drained_run_offers_no_drain() {
    let mut idx = index(false);
    idx.progress.draining = true;
    let page = render_body(&idx, &live());
    assert!(page.contains("(draining)"));
    assert!(!page.contains("action=\"/drain\""));
}

#[test]
fn the_series_lists_the_last_windows_newest_first_with_their_events() {
    let page = render_body(&index(false), &RenderOpts::default());
    let table = &page[page.find("<th>window</th>").unwrap()..];
    let table = &table[..table.find("</table>").unwrap()];
    let rows: Vec<&str> = table.split("<tr>").skip(1).collect();
    assert_eq!(rows.len(), series::TABLE_WINDOWS, "the last 20 windows");
    assert!(rows[0].starts_with("<td>2:05–2:10</td>"), "newest first: {}", rows[0]);
    assert!(rows[19].starts_with("<td>0:30–0:35</td>"), "oldest listed: {}", rows[19]);
    let marked = rows.iter().find(|r| r.contains("rate 15 → 10")).expect("event marked");
    assert!(marked.starts_with("<td>1:50–1:55</td>"), "on the window holding it: {marked}");
    assert_eq!(page.matches("<svg").count(), 7, "one sparkline per series");
    assert!(page.contains("25 window(s) over 2:10"), "the whole run is covered");
}

#[test]
fn a_merged_window_shows_its_whole_span() {
    let mut idx = index(false);
    idx.series.truncate(3);
    let page = render_body(&idx, &RenderOpts::default());
    assert!(page.contains("<td>0:00–0:10</td>"), "the merged 10 s window: {page}");
    assert!(page.contains("rate 20 → 15"), "its event is marked");
}

#[test]
fn rate_events_are_listed_with_the_count_past_the_bound() {
    let page = render_body(&index(false), &RenderOpts::default());
    let events = &page[page.find("<h2>Rate events</h2>").unwrap()..];
    assert!(events.contains("<td>0:04</td><td>rate 20 → 15</td>"));
    assert!(events.contains("<td>1:52</td><td>rate 15 → 10</td>"));
    assert!(events.contains("+3 more"));
}

#[test]
fn the_last_failures_are_newest_first_and_link_their_sample() {
    let opts = RenderOpts { sample_prefix: "/load/run-1/files/".into(), live: None };
    let page = render_body(&index(false), &opts);
    let fails = &page[page.find("<h2>Last failures</h2>").unwrap()..];
    let (first, second) = (fails.find("status_486").unwrap(), fails.find("timeout").unwrap());
    assert!(first < second, "newest first");
    assert!(fails.contains(
        "<a href=\"/load/run-1/files/callflows/basic_call/status_486/alice@answered/0.html\">callflow</a>"
    ));
}

#[test]
fn the_error_column_cuts_the_detail_and_titles_it_whole() {
    let page = render_body(&index(false), &RenderOpts::default());
    let full = format!("alice expected 200, got 486 &lt;Busy&gt; {}", "x".repeat(300));
    assert!(page.contains(&format!("title=\"{full}\"")), "the full text, escaped, as title");
    let shown = format!(">alice expected 200, got 486 &lt;Busy&gt; {}…</span>", "x".repeat(165));
    assert!(page.contains(&shown), "cut to 200 characters: {page}");
    let audit_row = &page[page.find("rule-x").unwrap()..];
    let audit_row = &audit_row[..audit_row.find("</tr>").unwrap()];
    assert!(audit_row.contains("no trace"), "a failure bucket without a sample: {audit_row}");
    assert!(audit_row.contains("2 of 30 audited (6.7 %), ≈ 43 of 640"), "{audit_row}");
    assert!(audit_row.contains("none recorded (1 in 64 calls is recorded)"));
}

#[test]
fn the_recording_line_states_the_gates() {
    let page = render_body(&index(false), &RenderOpts::default());
    assert!(page.contains(
        "<b>basic_call</b>: 693 calls placed, 40 recorded (the first 10, 1 in 64, 64 after each \
         new failure mode). 640 ended on an accepted branch; the RFC audit judged the recording \
         of 30 of them."
    ));
}

#[test]
fn the_status_line_reads_the_window_just_closed() {
    let idx = index(false);
    let w = window(30, 5, 97, 3);
    assert_eq!(
        status_line(&idx.progress, 60, &w),
        "elapsed 0:35 / 1:00 · 19.8 cps (target 10) · in flight 41 · ok 640 fail 9 rej 3 · \
         error 3.0 % (last 5 s)"
    );
}
