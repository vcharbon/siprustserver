//! `loadgen` CLI — drive the load from the host against the SUT, multiplexing all
//! dialogs over a few static endpoint sockets.
//!
//! Example (host → kind VIP, our b2bua routing the b-leg to the static uas addr):
//! ```text
//! cargo run -p loadgen --release -- \
//!   --cps 50 --duration 600 --max-in-flight 4000 \
//!   --target 172.20.255.250:5060 --bind-ip 172.20.0.1 \
//!   --correlation-header X-Loadgen-Id --route-pin-to-uas \
//!   --out-dir ./loadgen-report
//! ```
//!
//! See `crates/loadgen/README.md` for the quick start, how it relates to the
//! existing test suites, and how to add scenarios.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use loadgen::{
    by_id, default_scenarios, serve_metrics, CallConfig, Correlation, Driver, DriverCfg,
    EndpointSpec, LoadScenario, MuxCore, MuxTransport, Reporter, ReporterCfg, Role,
};

#[derive(Parser)]
#[command(name = "loadgen", about = "SIP load generator (multiplexed SIPp substitute)")]
struct Args {
    /// Offered call rate (calls per second).
    #[arg(long, default_value_t = 10.0)]
    cps: f64,
    /// How long to offer load, in seconds.
    #[arg(long, default_value_t = 60)]
    duration: u64,
    /// Max concurrent in-flight calls (offered load above this is dropped+counted).
    #[arg(long, default_value_t = 2000)]
    max_in_flight: usize,
    /// The SUT address the INVITE routes through (front-proxy VIP).
    #[arg(long, default_value = "172.20.255.250:5060")]
    target: SocketAddr,
    /// Local host IP to bind the mux endpoints on (reachable from the SUT).
    #[arg(long, default_value = "127.0.0.1")]
    bind_ip: IpAddr,
    /// Base UDP port: uac=base, uas=base+1, refer=base+2.
    #[arg(long, default_value_t = 6000)]
    base_port: u16,
    /// Correlation header name: the transparent per-call token header the SUT
    /// relays onto every leg (no To-/R-URI fallback — header-only is SUT-agnostic).
    #[arg(long, default_value = "X-Loadgen-Id")]
    correlation_header: String,
    /// Attach an X-Api-Call destination pin to route the b-leg to the static uas
    /// endpoint (our-b2bua adapter). Off → the SUT routes the callee itself.
    #[arg(long, default_value_t = false)]
    route_pin_to_uas: bool,
    /// Per-recv wall-clock timeout (ms).
    #[arg(long, default_value_t = 5000)]
    recv_timeout_ms: u64,
    /// Max stored callflow samples per (scenario, result-class).
    #[arg(long, default_value_t = 10)]
    sample_cap: u32,
    /// Output directory for the on-disk report.
    #[arg(long, default_value = "./loadgen-report")]
    out_dir: PathBuf,
    /// Address to serve the Prometheus /metrics endpoint on.
    #[arg(long, default_value = "0.0.0.0:9300")]
    metrics_addr: SocketAddr,
    #[arg(long, default_value_t = 60)]
    options_hold: u64,
    #[arg(long, default_value_t = 5)]
    options_cadence: u64,
    /// Realistic ring time (ms): callee dwell between 180 and 200. 0 = immediate.
    #[arg(long, default_value_t = 0)]
    ring_delay_ms: u64,
    /// Post-connect talk time (ms) held before BYE on a basic call. 0 = immediate.
    #[arg(long, default_value_t = 0)]
    talk_time_ms: u64,
    /// Spacing (ms) held before and after a re-INVITE. 0 = back-to-back.
    #[arg(long, default_value_t = 0)]
    reinvite_gap_ms: u64,
    /// Total hold (seconds) of the `long_call` scenario, around its OPTIONS ping.
    #[arg(long, default_value_t = 1200)]
    long_hold_secs: u64,
    #[arg(long, default_value = "refer-allow-c")]
    refer_key: String,
    /// Record roughly 1 call in N's flow (the sampling-gate background fraction).
    /// `1` = full recording (every call); higher values record less. Stored
    /// samples stay bounded by `--sample-cap` per bucket; the cost of full
    /// recording is per-call recording memory, so watch `loadgen_process_*` /
    /// `loadgen_inflight` when it is on.
    #[arg(long, default_value_t = 64)]
    background_record_every: u64,
    /// Periodically re-write the on-disk report every N seconds during the run
    /// (so it is browsable mid-run, not only at exit). 0 = only at the end.
    #[arg(long, default_value_t = 0)]
    report_interval_secs: u64,
    /// Scenario weights, repeatable: `--scenario basic_call=4 --scenario refer=1`.
    #[arg(long = "scenario")]
    scenarios: Vec<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    let scenarios: Vec<(Arc<dyn LoadScenario>, f64)> = if args.scenarios.is_empty() {
        default_scenarios()
    } else {
        args.scenarios
            .iter()
            .map(|spec| {
                let (name, weight) = spec
                    .split_once('=')
                    .map(|(n, w)| (n, w.parse::<f64>().unwrap_or(1.0)))
                    .unwrap_or((spec.as_str(), 1.0));
                let s = by_id(name).unwrap_or_else(|| panic!("unknown scenario {name:?}"));
                (s, weight)
            })
            .collect()
    };

    let correlation = Correlation::header(args.correlation_header.clone());

    let uac: SocketAddr = (args.bind_ip, args.base_port).into();
    let uas: SocketAddr = (args.bind_ip, args.base_port + 1).into();
    let refer: SocketAddr = (args.bind_ip, args.base_port + 2).into();
    let recv_timeout = Duration::from_millis(args.recv_timeout_ms);

    let core = MuxCore::bind(
        vec![
            EndpointSpec { addr: uac, role: Role::Caller },
            EndpointSpec { addr: uas, role: Role::Callee },
            EndpointSpec { addr: refer, role: Role::Callee },
        ],
        correlation.clone(),
        256,
        args.sample_cap as usize,
        recv_timeout,
    )
    .await?;

    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: uac,
        uas_addr: uas,
        refer_addr: refer,
        correlation,
        recv_timeout,
    });

    let reporter = Arc::new(Reporter::new(ReporterCfg {
        sample_cap: args.sample_cap,
        // 1 = full recording (every call); the default (64) keeps the converging
        // background sampling gate.
        background_record_every: args.background_record_every,
    }));

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        .max(1);

    let cfg = DriverCfg {
        cps: args.cps,
        duration: Duration::from_secs(args.duration),
        max_in_flight: args.max_in_flight,
        seed,
        call: CallConfig {
            via: args.target,
            route_pin: args.route_pin_to_uas.then_some(uas),
            refer_pin: args.route_pin_to_uas.then_some(refer),
            refer_key: args.refer_key.clone(),
            options_hold: Duration::from_secs(args.options_hold),
            options_cadence: Duration::from_secs(args.options_cadence),
            ring_delay: Duration::from_millis(args.ring_delay_ms),
            talk_time: Duration::from_millis(args.talk_time_ms),
            reinvite_gap: Duration::from_millis(args.reinvite_gap_ms),
            long_hold: Duration::from_secs(args.long_hold_secs),
            teardown_quiesce: Duration::from_millis(250),
        },
    };

    let driver = Driver::new(cfg, scenarios, reporter.clone(), transport);

    // Live /metrics: reporter series + mux series + a process-memory canary
    // (RSS) so the endurance dashboard can watch the load generator itself while
    // full recording is on.
    let metrics_reporter = reporter.clone();
    let metrics_core = core.clone();
    let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
        format!(
            "{}{}{}",
            metrics_reporter.render_prometheus(),
            metrics_core.render_prometheus(),
            process_memory_metrics(),
        )
    });
    let metrics_addr = args.metrics_addr;
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(metrics_addr, render).await {
            eprintln!("[loadgen] /metrics server stopped: {e}");
        }
    });

    // Periodically snapshot the on-disk report so it is browsable mid-run (the
    // endurance harness copies it out without waiting for the job to finish).
    if args.report_interval_secs > 0 {
        let snap_reporter = reporter.clone();
        let out = args.out_dir.clone();
        let every = Duration::from_secs(args.report_interval_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(e) = snap_reporter.finalize(&out) {
                    eprintln!("[loadgen] periodic report snapshot failed: {e}");
                }
            }
        });
    }

    eprintln!(
        "[loadgen] {} cps for {}s, max_in_flight={}, target={}, uas={}, /metrics on {}",
        args.cps, args.duration, args.max_in_flight, args.target, uas, args.metrics_addr
    );
    driver.run().await;

    reporter.finalize(&args.out_dir)?;
    eprintln!(
        "[loadgen] done: {} calls; registry_size={}; report at {}",
        reporter.total_calls(),
        core.registry_size(),
        args.out_dir.display()
    );
    println!("{}", reporter.render_prometheus());
    println!("{}", core.render_prometheus());
    Ok(())
}

/// A process resident-memory canary in Prometheus format, read from
/// `/proc/self/statm` (field 2 = resident pages × page size). The load generator
/// holds per-call recording buffers while full recording is on, so the endurance
/// dashboard watches this to catch a recording-memory blow-up early. Returns an
/// empty string off Linux / if the file is unreadable (best effort).
fn process_memory_metrics() -> String {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).map(|w| w.to_string()))
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages.saturating_mul(4096));
    match rss {
        Some(bytes) => format!(
            "# HELP loadgen_process_resident_memory_bytes Load generator RSS.\n\
             # TYPE loadgen_process_resident_memory_bytes gauge\n\
             loadgen_process_resident_memory_bytes {bytes}\n"
        ),
        None => String::new(),
    }
}
