//! The live sampler and the panic-ELU gate on a real multi-thread runtime whose
//! workers outnumber its CPU quota: the shape of a pod whose runtime was sized
//! past its limit, or of any sub-core limit (one worker, a fraction of a core).
//! Real time and real CPU, so the offered loads sit well clear of the 0.75
//! threshold on both sides.

use super::cpu_budget::CpuBudget;
use super::sampler::LiveLoadSampler;
use super::*;
use crate::new_calls::Refusal;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKERS: usize = 4;
/// CPU burnt by one offered task.
const TASK: Duration = Duration::from_micros(200);
/// Sampler ticks per run: 1.5 s of load, enough for the EWMA (α 0.2) to settle.
const TICKS: u32 = 15;

fn spin(d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d {
        std::hint::spin_loop();
    }
}

/// Offer `cores` of CPU-bound work to a `WORKERS`-worker runtime while a signal
/// built on it by `sampler` is sampled every [`OverloadSignal::SAMPLE_PERIOD`],
/// then return the signal. The sampler is built on a runtime worker, so it
/// reads that runtime's metrics.
fn run_offered(cores: f64, sampler: fn() -> OverloadSignal) -> OverloadSignal {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_all()
        .build()
        .expect("runtime");
    let sig = rt.block_on(async move {
        tokio::spawn(async move {
            let sig = sampler();
            sig.configure_admission(&crate::config::B2buaConfig {
                cps_bucket_size: 1000,
                cps_bucket_rate: 0,
                overload_panic_elu_threshold: 0.75,
                ..Default::default()
            });
            // Tasks owed follow real elapsed time, so a delayed tick still
            // offers the full rate.
            let generator = tokio::spawn(async move {
                let start = Instant::now();
                let per_sec = cores / TASK.as_secs_f64();
                let mut spawned = 0u64;
                let mut tick = tokio::time::interval(Duration::from_millis(1));
                loop {
                    tick.tick().await;
                    let owed = (start.elapsed().as_secs_f64() * per_sec) as u64;
                    for _ in spawned..owed {
                        tokio::spawn(async { spin(TASK) });
                    }
                    spawned = owed.max(spawned);
                }
            });
            let mut tick = tokio::time::interval(OverloadSignal::SAMPLE_PERIOD);
            tick.tick().await;
            for _ in 0..TICKS {
                tick.tick().await;
                sig.sample();
            }
            generator.abort();
            sig
        })
        .await
        .expect("load task")
    });
    rt.shutdown_background();
    sig
}

/// A one-core quota over an affinity set with a CPU for every worker.
fn one_core_quota() -> OverloadSignal {
    fn budget() -> Option<CpuBudget> {
        Some(CpuBudget { quota: Some(1.0), affinity: Some(64) })
    }
    OverloadSignal::new(Arc::new(LiveLoadSampler::with_budget(budget)))
}

/// 0.9 of a one-core quota spread over four workers is overload of the CPU the
/// runtime may use: the busy ratio reads against the quota, not the four
/// workers, and the panic backstop sheds a non-emergency call.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn panic_elu_sheds_when_offered_work_nears_the_cpu_quota() {
    let sig = run_offered(0.9, one_core_quota);
    let elu = sig.metrics().elu_ewma;
    assert!(elu > 0.75, "elu {elu} must read against a one-core quota, not {WORKERS} workers");
    assert_eq!(
        super::tests::admit(&sig, false).err().map(|r| r.reason),
        Some(Refusal::PanicElu),
        "elu {elu}"
    );
}

/// A light load against the same quota reads light and admits.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn light_load_on_the_cpu_quota_admits() {
    let sig = run_offered(0.2, one_core_quota);
    let elu = sig.metrics().elu_ewma;
    assert!(elu < 0.6, "elu {elu}");
    assert!(super::tests::admit(&sig, false).is_ok(), "elu {elu}");
}

/// The production path end to end: the quota read from this process's own
/// cgroup. Meaningful only under a one-core quota, hence ignored and run by
/// `just test-cpu-quota`.
#[test]
#[ignore = "needs a one-core cgroup CPU quota on the test process"]
fn live_signal_sheds_under_a_real_one_core_quota() {
    let sig = run_offered(0.9, OverloadSignal::live);
    let elu = sig.metrics().elu_ewma;
    assert!(elu > 0.75, "elu {elu}");
    assert_eq!(
        super::tests::admit(&sig, false).err().map(|r| r.reason),
        Some(Refusal::PanicElu),
        "elu {elu}"
    );
}
