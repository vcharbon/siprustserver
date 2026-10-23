//! The reusable `loadgen` CLI application: arg parsing, load-profile
//! precedence, mix resolution, mux/driver wiring, the metrics socket and the
//! report writer, behind the [`Product`] seam. A product binary supplies its
//! name, its own flags and the [`ShapeRegistry`](crate::ShapeRegistry) they select, and calls
//! [`main`]; it adds no wiring of its own (see [`product`]).

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;

use crate::case::CaseLoader;
use crate::outcome::OutcomeSink;
use crate::{
    serve_metrics, CallConfig, Canaries, Driver, DriverCfg, EndpointSpec, LoadRunMeta, MuxCore,
    MuxTransport, RateHandle, Reporter, ReporterCfg, Role, ScenarioInputs,
};

mod args;
mod mix;
pub mod product;

pub use args::Args;
use args::{base_tuning, correlation, endpoint_config, outcome_sink};
pub use mix::check_from_user_mix;
use mix::{resolve_mix, Mix};
pub use product::{command, main, parse_from, NoFlags, Product, RunLayout, Shipped};

/// The file of `--out-dir` that holds the `/metrics` body as the run ended.
pub const FINAL_METRICS_FILE: &str = "metrics.prom";

/// Run the complete loadgen application for `product`: its `flags` and the
/// loadgen `args` parsed (and configured) by [`main`]. The registry is the
/// product's, built once the endpoint layout is resolved; a registry the
/// product cannot build fails the run before any socket is bound.
pub async fn run<P: Product>(product: &P, flags: &P::Flags, args: Args) -> std::io::Result<()> {
    // Per-run scenario inputs (SUT auth data, not topology): consumed at
    // scenario CONSTRUCTION (e.g. the refer scenarios' `refer_key`).
    let inputs = ScenarioInputs { refer_key: args.refer_key.clone() };
    // The optional LoadProfile: its fields are DEFAULTS, overridden by any
    // explicitly-passed CLI flag (see `Args::explicit`). Absent = flag-only
    // behaviour, byte-for-byte.
    let profile = args
        .load_profile
        .as_deref()
        .map(|p| {
            e2e_model::load_load_profile(p)
                .unwrap_or_else(|e| panic!("--load-profile {}: {e}", p.display()))
        })
        .unwrap_or_default();

    // Resolve each profile-overridable scalar: explicit flag wins, else profile.
    let cps = if args.explicit("cps") { args.cps } else { profile.cps };
    let duration = if args.explicit("duration") { args.duration } else { profile.duration_secs };
    let max_in_flight =
        if args.explicit("max_in_flight") { args.max_in_flight } else { profile.max_in_flight };
    let overload_reject = if args.explicit("overload_reject") {
        args.overload_reject
    } else {
        profile.overload_reject
    };
    let sample_cap = if args.explicit("sample_cap") { args.sample_cap } else { profile.sample_cap };
    let background_record_every = if args.explicit("background_record_every") {
        args.background_record_every
    } else {
        profile.background_record_every
    };
    let report_interval_secs = if args.explicit("report_interval_secs") {
        args.report_interval_secs
    } else {
        profile.report_interval_secs
    };
    // The recv timeout feeds the FLAG-synthesized endpoint config only (an authored
    // `--endpoint-config` carries its own `recvTimeoutMs`, which stays the source of
    // truth for the environment axis).
    let recv_timeout_ms = if args.explicit("recv_timeout_ms") {
        args.recv_timeout_ms
    } else {
        profile.recv_timeout_ms
    };

    // ONE process-wide monotonic-anchored clock, created here and shared with the
    // mux and every per-call binder — so all call timelines ride a single axis
    // (and loadgen reads no raw SystemTime on its own timeline). See `sip_clock::Clock`.
    let clock = Clock::system();

    // Per-run RNG seed off the shared clock (varies per run; no raw SystemTime).
    // Also seeds the Test-case binding resolvers (`${rand:N}` + random pool walk).
    let seed = (clock.now_ms() as u64).max(1);

    let base_tuning = base_tuning(&args, &profile);
    // The shared Check-set store a case's `checkSets` resolve against (the
    // same directory the e2e runner reads; missing dir = empty store).
    let check_sets = e2e_model::load_check_sets(&args.check_sets_dir)
        .unwrap_or_else(|e| panic!("--check-sets-dir {}: {e}", args.check_sets_dir.display()));
    // The ONE environment-axis document (authored file or flag-synthesized):
    // endpoint binds + SUT ingress + recv bound + egress policy.
    let endpoint = endpoint_config(&args, recv_timeout_ms);
    let registry = product
        .registry(flags, &RunLayout { endpoint: &endpoint })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if args.list_shapes {
        registry.iter().filter(|d| d.load.is_some()).for_each(|d| println!("{}", d.id));
        return Ok(());
    }
    let mut cases = CaseLoader::new(&check_sets, seed);
    let Mix { entries: scenarios, tuning } =
        resolve_mix(&args, &profile, &registry, &inputs, base_tuning, &mut cases);

    let correlation = correlation(&args).unwrap_or_else(|e| panic!("{e}"));
    check_from_user_mix(&correlation, &scenarios).unwrap_or_else(|e| panic!("{e}"));
    let name = product.name();
    let mix_ids: Vec<&str> = scenarios.iter().map(|e| e.id).collect();

    let (uac, uas, refer) =
        (endpoint.addr("alice"), endpoint.addr("bob"), endpoint.addr("charlie"));
    let via = endpoint.addr("lb");
    let egress = endpoint.egress_policy();
    let recv_timeout = endpoint.recv_timeout();

    let core = MuxCore::bind(
        vec![
            EndpointSpec { addr: uac, role: Role::Caller },
            EndpointSpec { addr: uas, role: Role::Callee },
            EndpointSpec { addr: refer, role: Role::Callee },
        ],
        correlation.clone(),
        256,
        sample_cap as usize,
        recv_timeout,
        clock.clone(),
    )
    .await?;
    // The transport addresses the bound endpoints (a port-0 request resolves).
    let bound = |requested| core.local_addr(requested).expect("every role's address is bound");
    let (uac, uas, refer) = (bound(uac), bound(uas), bound(refer));

    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: uac,
        uas_addr: uas,
        refer_addr: refer,
        correlation,
        recv_timeout,
        clock: clock.clone(),
    });

    let reporter = Arc::new(Reporter::new(ReporterCfg {
        sample_cap,
        // 1 = full recording (every call); the default (64) keeps the converging
        // background sampling gate.
        background_record_every,
    }));
    reporter.set_stream(&args.stream);
    core.set_stream(&args.stream);

    let cfg = DriverCfg {
        cps,
        duration: Duration::from_secs(duration),
        max_in_flight,
        seed,
        call: CallConfig {
            via,
            egress: egress.clone(),
            options_hold: Duration::from_secs(args.options_hold),
            options_cadence: Duration::from_secs(args.options_cadence),
            ring_delay: Duration::from_millis(args.ring_delay_ms),
            talk_time: Duration::from_millis(args.talk_time_ms),
            reinvite_gap: Duration::from_millis(args.reinvite_gap_ms),
            long_hold: Duration::from_secs(args.long_hold_secs),
            teardown_quiesce: Duration::from_millis(250),
        },
        default_tuning: base_tuning,
        tuning,
        overload_reject,
    };

    reporter.declare_scenarios(scenarios.iter().map(|entry| entry.id));
    let outcomes = Arc::new(outcome_sink(&args, name, clock.now_ms())?);
    let driver =
        Driver::new(cfg, scenarios, reporter.clone(), transport).with_outcomes(outcomes.clone());
    // The live rate handle (seeded from `--cps` / the profile): `POST /rate`
    // re-targets it and the governor re-anchors its grid; exported as the
    // `loadgen_target_cps` gauge.
    let rate = driver.rate_handle();
    // SIGUSR1 drains the run, as `POST /drain` does: no new call, the calls in
    // flight end, then the run finishes as at its duration.
    let usr1_rate = rate.clone();
    let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    tokio::spawn(async move {
        if usr1.recv().await.is_some() {
            usr1_rate.drain();
        }
    });

    // Live /metrics: reporter series + mux series + the current rate target + a process-memory canary (RSS) so the loadgen dashboard can
    // watch the load generator itself while full recording is on.
    let metrics_reporter = reporter.clone();
    let metrics_core = core.clone();
    let metrics_rate = rate.clone();
    let metrics_outcomes = outcomes.clone();
    let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
        metrics_body(&metrics_reporter, &metrics_core, &metrics_rate, &metrics_outcomes)
    });
    let metrics_addr = args.metrics_addr;
    let server_rate = rate.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(metrics_addr, render, Some(server_rate)).await {
            eprintln!("[{name}] /metrics server stopped: {e}");
        }
    });

    // The run-metadata template for the machine-readable `load-result.json`: the
    // echoed knobs (timing filled per-write). Shared with the periodic snapshot
    // task; each write clones it and stamps `finished`/`finished_ms`.
    let started_ms = clock.now_ms();
    let meta_base = LoadRunMeta {
        started_ms,
        finished_ms: started_ms,
        finished: false,
        target: via.to_string(),
        cps,
        duration_secs: duration,
        max_in_flight: max_in_flight as u64,
        egress: Some(egress_label(&egress)),
        profile: profile.description.clone(),
    };
    let meta_for = |clock: &Clock, finished: bool| LoadRunMeta {
        finished_ms: clock.now_ms(),
        finished,
        ..meta_base.clone()
    };

    // Periodically snapshot the on-disk report so it is browsable mid-run. Each
    // snapshot rewrites index.html/summary.md AND the machine-readable
    // load-result.json (still `finished:false`), listing the same sample pages.
    // The handle is aborted+awaited before the final write below (see
    // `Reporter::spawn_snapshots` shutdown-ordering contract).
    let snapshots = (report_interval_secs > 0).then(|| {
        let snap_clock = clock.clone();
        let snap_meta = meta_base.clone();
        let snap_core = core.clone();
        reporter.spawn_snapshots(
            args.out_dir.clone(),
            Duration::from_secs(report_interval_secs),
            move || LoadRunMeta {
                finished_ms: snap_clock.now_ms(),
                finished: false,
                ..snap_meta.clone()
            },
            move || mux_canaries(&snap_core),
        )
    });

    eprintln!(
        "[{name}] {cps} cps for {duration}s, max_in_flight={max_in_flight}, target={via}, uas={uas}, egress={egress:?}, shapes={:?}, /metrics on {}",
        mix_ids,
        args.metrics_addr
    );
    driver.run().await;

    // Stop the snapshot task BEFORE the final write: aborting and awaiting the
    // handle guarantees no `finished:false` snapshot lands after (or interleaves
    // with) the `finished:true` index — a tick fired exactly at the run's last
    // instant (report interval dividing the duration) otherwise races it.
    if let Some(snap) = snapshots {
        snap.abort();
        let _ = snap.await;
    }
    reporter.finalize_run(&args.out_dir, meta_for(&clock, true), mux_canaries(&core))?;
    outcomes.close();
    // The final `/metrics` body, for whoever reads the run's counters once the
    // process is gone.
    std::fs::write(
        args.out_dir.join(FINAL_METRICS_FILE),
        metrics_body(&reporter, &core, &rate, &outcomes),
    )?;
    eprintln!(
        "[{name}] done: {} offered: {} records, {} not placed, {} generator failures{}; \
         registry_size={}; report at {}",
        outcomes.offered_total(),
        outcomes.records_written(),
        outcomes.not_placed_total(),
        outcomes.generator_failures_total(),
        if outcomes.reconciles() { "" } else { " (UNACCOUNTED calls)" },
        core.registry_size(),
        args.out_dir.display()
    );
    println!("{}", reporter.render_prometheus());
    println!("{}", core.render_prometheus());
    Ok(())
}

/// A short, stable label for the run's egress policy (the `meta.egress` field of
/// the machine-readable index): `transparent` / `api-call-pin` / `registrar-aor`.
fn egress_label(egress: &crate::EgressPolicy) -> String {
    use crate::EgressPolicy;
    match egress {
        EgressPolicy::Transparent => "transparent".to_string(),
        EgressPolicy::ApiCallPin => "api-call-pin".to_string(),
        EgressPolicy::RegistrarAor { domain } => format!("registrar-aor:{domain}"),
    }
}

/// The mux-owned run canaries (orphans + simulated-loss drops) for the
/// machine-readable index. The reporter fills in its own shed + ringing halves
/// in `build_index`, so these two totals are all the caller supplies.
fn mux_canaries(core: &MuxCore) -> Canaries {
    use std::sync::atomic::Ordering;
    let s = core.stats();
    let orphans = s.orphans_total();
    let drops = s.dropped_out.load(Ordering::Relaxed) + s.dropped_in.load(Ordering::Relaxed);
    Canaries { orphans, drops, ..Canaries::default() }
}

/// The `/metrics` body, every family of [`crate::CATALOGUE`]: the
/// reporter's series, the mux's, the offered-rate target
/// (what `POST /rate` last set, `0` while paused) and a process
/// resident-memory canary — the load generator holds per-call recording
/// buffers while full recording is on, so the loadgen dashboard watches it
/// to catch a recording-memory blow-up early.
pub fn metrics_body(
    reporter: &Reporter,
    core: &MuxCore,
    rate: &RateHandle,
    outcomes: &OutcomeSink,
) -> String {
    let stream = reporter.stream();
    let mut out = reporter.render_prometheus();
    out.push_str(&core.render_prometheus());
    crate::catalogue::TARGET_CPS.render_rows(&mut out, [([stream.as_str()], rate.cps())]);
    let rss = resident_memory_bytes().map_or("NaN".to_string(), |b| b.to_string());
    crate::catalogue::PROCESS_RESIDENT_MEMORY.render_rows(&mut out, [([stream.as_str()], rss)]);
    outcomes.render_prometheus(&mut out);
    out
}

/// The process's resident memory from `/proc/self/statm` (field 2 =
/// resident pages × page size); `None` off Linux or if unreadable.
fn resident_memory_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).map(|w| w.to_string()))
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages.saturating_mul(4096))
}

#[cfg(test)]
mod tests;
