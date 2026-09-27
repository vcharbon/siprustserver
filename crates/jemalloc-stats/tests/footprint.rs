//! The allocator's footprint under a transaction-table allocation pattern on a
//! multi-thread runtime, per resolved jemalloc configuration, and the defaults
//! a binary linking this crate resolves with no operator setting.
//!
//! jemalloc reads `_RJEM_MALLOC_CONF` once, before `main`, so each case
//! re-executes this test binary with the configuration under test and reads
//! the child's report. The pattern: every runtime worker allocates ~3 KiB
//! records (the size class of a boxed SIP transaction) with a few small
//! strings, each freed on whichever worker its timer lands on after a short
//! or a long lifetime, held at a fixed live count.
//!
//! The bound: at the recommended profiling interval (2^19, jemalloc's default)
//! or with profiling off, `active` stays within 1.3× `allocated` at the held
//! heap. The lane's diagnostic interval (2^13) is kept as the falsifier of the
//! hazard model: a sample of a small object costs two pages, so it doubles the
//! small heap; the startup line must say so. The pattern holds its heap for
//! 1.5 s, too short for slab slack to build: it cannot tell arena counts apart
//! (1.00 at one arena, 1.09 at 97), which minutes of churn do (ADR-0038).

#![cfg(not(target_env = "msvc"))]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const CHILD: &str = "JEMALLOC_FOOTPRINT_CHILD";
const RECORD_BYTES: usize = 3000;
const LIVE_RECORDS: usize = 40_000;
const BATCH: usize = 64;
const SHORT: Duration = Duration::from_millis(40);
const LONG: Duration = Duration::from_millis(320);
const HOLD: Duration = Duration::from_millis(1500);
const FILL_LIMIT: Duration = Duration::from_secs(60);

struct Record {
    _body: Vec<u8>,
    _tag: String,
    _via: String,
}

fn record(i: usize) -> Record {
    Record {
        _body: vec![i as u8; RECORD_BYTES],
        _tag: format!("z9hG4bK-{i:032x}"),
        _via: "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK;rport=5060;received=10.0.0.1".repeat(6),
    }
}

/// Runs the pattern to its held heap and prints `allocated active` in bytes.
/// Frees `batch` on whichever worker the timer lands on.
fn drop_after(batch: Vec<Record>, lifetime: Duration, live: Arc<AtomicUsize>) {
    tokio::spawn(async move {
        tokio::time::sleep(lifetime).await;
        drop(batch);
        live.fetch_sub(BATCH, Ordering::Relaxed);
    });
}

fn lifetime(n: usize) -> Duration {
    if n.is_multiple_of(2) {
        SHORT
    } else {
        LONG
    }
}

fn child_pattern() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let workers = rt.metrics().num_workers();
    rt.block_on(async {
        // Fill to the held count by count, on the workers, before any timer
        // runs: the count then depends on no allocation rate, so a slow
        // deployment (a fractional CPU quota, a backtrace per sample) holds
        // the same heap as a fast one.
        let mut held = Vec::new();
        let mut n = 0;
        while n < LIVE_RECORDS {
            let batches: Vec<_> = (0..workers)
                .map(|w| {
                    tokio::spawn(async move {
                        (0..BATCH).map(|i| record(n + w * BATCH + i)).collect::<Vec<_>>()
                    })
                })
                .collect();
            for b in batches {
                held.push((b.await.unwrap(), lifetime(n)));
                n += BATCH;
            }
        }
        live.store(n, Ordering::Relaxed);
        for (batch, lt) in held {
            drop_after(batch, lt, live.clone());
        }
        // Churn: every worker replaces what its timers free, at its own rate.
        let mut producers = Vec::new();
        for w in 0..workers {
            let live = live.clone();
            producers.push(tokio::spawn(async move {
                let mut n = w;
                loop {
                    if live.load(Ordering::Relaxed) >= LIVE_RECORDS {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                    let batch: Vec<Record> = (0..BATCH).map(|i| record(n + i)).collect();
                    n += BATCH;
                    live.fetch_add(BATCH, Ordering::Relaxed);
                    drop_after(batch, lifetime(n), live.clone());
                    tokio::task::yield_now().await;
                }
            }));
        }
        tokio::time::sleep(HOLD).await;
        // Measure at an instant the count is held (a slow churn dips below it).
        let start = std::time::Instant::now();
        while live.load(Ordering::Relaxed) < LIVE_RECORDS * 9 / 10 && start.elapsed() < FILL_LIMIT {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (allocated, active) = jemalloc_stats::footprint_bytes().expect("jemalloc answers");
        println!("FOOTPRINT {allocated} {active} {}", live.load(Ordering::Relaxed));
        for p in &producers {
            p.abort();
        }
    });
}

/// The output of this binary re-run as the child for `test` under `conf`
/// (`None`: no `_RJEM_MALLOC_CONF` at all).
fn child(test: &str, conf: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", test, "--nocapture", "--test-threads=1"]).env(CHILD, "1");
    match conf {
        Some(conf) => cmd.env("_RJEM_MALLOC_CONF", conf),
        None => cmd.env_remove("_RJEM_MALLOC_CONF"),
    };
    cmd.output().expect("child runs")
}

/// `(allocated, active, stderr)` of the pattern re-run as the child for
/// `test` under `conf`.
fn run_child(test: &str, conf: &str) -> (u64, u64, String) {
    let out = child(test, Some(conf));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // The harness prints `test <name> ... ` without a newline first, so the
    // report is found by its marker, not at a line start.
    let nums = stdout
        .split_whitespace()
        .skip_while(|w| *w != "FOOTPRINT")
        .skip(1)
        .take(2)
        .map(|w| w.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()
        .filter(|v| v.len() == 2)
        .map(|v| (v[0], v[1]))
        .unwrap_or_else(|| panic!("child printed no report: stdout={stdout} stderr={stderr}"));
    (nums.0, nums.1, stderr)
}

/// `active/allocated` of the child under `conf`, with at least `held` of
/// the pattern's heap allocated at the reading.
fn ratio(test: &str, conf: &str, held: f64) -> (f64, String) {
    let (allocated, active, stderr) = run_child(test, conf);
    let floor = (LIVE_RECORDS * RECORD_BYTES) as f64 * held;
    assert!(allocated as f64 > floor, "heap not held: {allocated} of {floor}");
    (active as f64 / allocated as f64, stderr)
}

#[test]
fn profiling_off_keeps_active_within_1_3_of_allocated() {
    if std::env::var_os(CHILD).is_some() {
        return child_pattern();
    }
    let (r, _) = ratio(
        "profiling_off_keeps_active_within_1_3_of_allocated",
        "prof:false,dirty_decay_ms:1000,muzzy_decay_ms:1000",
        0.5,
    );
    assert!(r <= 1.3, "active/allocated = {r:.2}");
}

#[test]
fn the_recommended_profiling_interval_keeps_active_within_1_3_of_allocated() {
    if std::env::var_os(CHILD).is_some() {
        return child_pattern();
    }
    let (r, _) = ratio(
        "the_recommended_profiling_interval_keeps_active_within_1_3_of_allocated",
        "prof:true,prof_active:true,lg_prof_sample:19,dirty_decay_ms:1000,muzzy_decay_ms:1000",
        0.5,
    );
    assert!(r <= 1.3, "active/allocated = {r:.2}");
}

/// The hazard model's falsifier: at 2^13 every third 3 KiB record is sampled
/// into a two-page extent of its own, outside `allocated`. A sample costs a
/// backtrace and an extent, so under a fractional CPU quota the churn holds
/// less of the heap; the ratio, not the held count, is the claim here.
#[test]
fn a_fine_profiling_interval_doubles_the_small_heap_and_the_startup_line_says_so() {
    if std::env::var_os(CHILD).is_some() {
        jemalloc_stats::log_config();
        return child_pattern();
    }
    let (r, stderr) = ratio(
        "a_fine_profiling_interval_doubles_the_small_heap_and_the_startup_line_says_so",
        "prof:true,prof_active:true,lg_prof_sample:13,dirty_decay_ms:1000,muzzy_decay_ms:1000",
        0.1,
    );
    assert!(r >= 1.7, "active/allocated = {r:.2}");
    let line = stderr.lines().find(|l| l.starts_with("jemalloc")).unwrap_or_default();
    assert!(line.contains("lg_prof_sample=13"), "startup line: {line}");
    assert!(line.contains("hazard"), "startup line names no hazard: {line}");
}

/// With no operator setting, a binary linking this crate resolves its
/// defaults: four arenas whatever the host, no huge-page refill, 1 s decays.
#[test]
fn a_binary_linking_the_crate_resolves_its_defaults_without_any_setting() {
    const TEST: &str = "a_binary_linking_the_crate_resolves_its_defaults_without_any_setting";
    if std::env::var_os(CHILD).is_some() {
        let r = jemalloc_stats::footprint_report().expect("jemalloc answers").resolved;
        println!(
            "RESOLVED conf={} opt.narenas={} thp={} dirty={} muzzy={}",
            r.malloc_conf.as_deref().unwrap_or("none"),
            r.opt_narenas,
            r.thp,
            r.dirty_decay_ms,
            r.muzzy_decay_ms
        );
        return;
    }
    let out = child(TEST, None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!(
        "RESOLVED conf={} opt.narenas=4 thp=never dirty=1000 muzzy=1000",
        jemalloc_stats::defaults::MALLOC_CONF
    );
    assert!(stdout.contains(&expected), "expected `{expected}` in: {stdout}");
    // An operator setting overrides key by key; the rest stay.
    let out = child(TEST, Some("narenas:2"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("opt.narenas=2 thp=never dirty=1000 muzzy=1000"), "{stdout}");
}
