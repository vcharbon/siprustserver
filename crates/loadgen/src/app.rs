//! The reusable `loadgen` CLI application: everything the shipped bin does —
//! arg parsing, load-profile precedence, mix resolution, mux/driver wiring, the
//! metrics socket, the report writer — behind ONE seam that takes the
//! [`ShapeRegistry`] (and, if needed, the per-run [`ScenarioInputs`]) as a
//! parameter instead of hardcoding [`ShapeRegistry::with_defaults`].
//!
//! This is the newkahneed-032 seam: ADR-0021 (b) made the shape registry OPEN
//! (`ShapeRegistry::empty().register(…)` lets a third-party crate add shapes
//! without patching the workspace), but the shipped bin constructed the
//! registry inline, so a downstream load bin's only options were forking the
//! whole bin or nothing. With this module a third-party bin is:
//!
//! ```ignore
//! #[tokio::main(flavor = "multi_thread")]
//! async fn main() -> std::io::Result<()> {
//!     loadgen::app::run(loadgen::app::Args::parse(), my_crate::registry()).await
//! }
//! ```
//!
//! and inherits every upstream CLI/profile/driver improvement for free; the
//! shipped `loadgen` bin is exactly that caller with
//! [`ShapeRegistry::with_defaults`].

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use e2e_model::{load_endpoint_config, EndpointConfig};
use sip_clock::Clock;

use crate::{
    serve_metrics, Canaries, CallConfig, CallTuning, ChaosLog, Correlation, Driver, DriverCfg,
    EndpointSpec, LoadCase, LoadRunMeta, MixEntry, MuxCore, MuxTransport, RateHandle, Reporter,
    ReporterCfg, Role, ScenarioInputs, ShapeRegistry,
};

/// The `loadgen` CLI surface. Parse with [`Args::parse`] (NOT
/// `clap::Parser::parse` — the inherent method also records which flags were
/// passed explicitly, the discriminator for the flag-overrides-profile
/// precedence). Fields are `pub` so a downstream bin may also construct one
/// programmatically; a field set that way only overrides a `--load-profile`
/// value if its clap id is inserted into [`Args::explicit`].
#[derive(Parser)]
#[command(name = "loadgen", about = "SIP load generator (multiplexed SIPp substitute)")]
pub struct Args {
    /// Path to a `LoadProfile` JSON (the complete declarative run spec: cps,
    /// duration, concurrency, sampling/report cadence, global loss/retransmit,
    /// recv timeout, and the scenario mix). Schema:
    /// `e2e/schemas/load-profile.schema.json`. PRECEDENCE: the profile supplies
    /// defaults; any explicitly-passed CLI flag OVERRIDES the profile value (so a
    /// profile pins a repeatable baseline and a one-off `--cps 30` tweaks it
    /// without editing the file). An explicit `--scenario` (or `--case`) replaces
    /// the profile's whole `mix`.
    #[arg(long)]
    pub load_profile: Option<PathBuf>,
    /// Offered call rate (calls per second).
    #[arg(long, default_value_t = 10.0)]
    pub cps: f64,
    /// How long to offer load, in seconds.
    #[arg(long, default_value_t = 60)]
    pub duration: u64,
    /// Max concurrent in-flight calls (offered load above this is dropped+counted).
    #[arg(long, default_value_t = 2000)]
    pub max_in_flight: usize,
    /// Path to an authored `EndpointConfig` JSON (the shared e2e-model
    /// environment axis): roles `alice` (UAC bind), `bob` (UAS bind), `charlie`
    /// (REFER-target bind), `lb` (the SUT ingress the INVITE routes through),
    /// plus `recvTimeoutMs` and the optional `egress` policy (`"transparent"` |
    /// `"api-call-pin"` | `{"registrar-aor":{"domain":…}}`). When set it is the
    /// single source of addressing/egress truth; `--target`/`--bind-ip`/
    /// `--base-port`/`--route-pin-to-uas`/`--recv-timeout-ms` stay as shorthand
    /// that synthesizes an equivalent config when it is absent.
    #[arg(long)]
    pub endpoint_config: Option<PathBuf>,
    /// Shorthand: the SUT address the INVITE routes through (front-proxy VIP).
    #[arg(long, default_value = "172.20.255.250:5060")]
    pub target: SocketAddr,
    /// Shorthand: local host IP to bind the mux endpoints on (reachable from the SUT).
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_ip: IpAddr,
    /// Shorthand: base UDP port: alice/uac=base, bob/uas=base+1, charlie/refer=base+2.
    #[arg(long, default_value_t = 6000)]
    pub base_port: u16,
    /// Correlation strategy: how the per-call token travels through the SUT.
    /// `header` (default): a transparent header the SUT must RELAY onto every
    /// leg (`--correlation-header`/`--correlation-template`; our b2bua:
    /// `B2BUA_RELAY_HEADERS`). `to-user`: the token IS the To-header user-part —
    /// survives any SIP-correct B2BUA with zero SUT cooperation.
    #[arg(long, default_value = "header")]
    pub correlate: String,
    /// Correlation header name (the `header` strategy): the transparent
    /// per-call token header the SUT relays onto every leg.
    #[arg(long, default_value = "X-Loadgen-Id")]
    pub correlation_header: String,
    /// Correlation header VALUE template with a `${token}` placeholder, so the
    /// token can ride a structured header — e.g. `"${token};encoding=hex"` for
    /// User-to-User, `"icid-value=${token}"` for P-Charging-Vector. Default:
    /// the bare token (byte-for-byte the historic behaviour).
    #[arg(long, default_value = "${token}")]
    pub correlation_template: String,
    /// Override the token-extraction regex (FIRST capture group = the token).
    /// Default: derived from `--correlation-template` (literal parts escaped,
    /// the placeholder matched as unreserved URI chars).
    #[arg(long)]
    pub correlation_extract: Option<String>,
    /// Shorthand for the `api-call-pin` egress policy: attach an X-Api-Call
    /// destination pin routing the b-leg to the static uas endpoint (our-b2bua
    /// adapter). Off → `transparent` (the SUT routes the callee itself).
    /// Ignored when `--endpoint-config` is set (its `egress` field wins).
    #[arg(long, default_value_t = false)]
    pub route_pin_to_uas: bool,
    /// Shorthand: per-recv wall-clock timeout (ms).
    #[arg(long, default_value_t = 5000)]
    pub recv_timeout_ms: u64,
    /// Max stored callflow samples per (scenario, result-class).
    #[arg(long, default_value_t = 10)]
    pub sample_cap: u32,
    /// Output directory for the on-disk report.
    #[arg(long, default_value = "./loadgen-report")]
    pub out_dir: PathBuf,
    /// Address to serve the Prometheus /metrics endpoint (GET) and the chaos-flag
    /// endpoint (`POST /chaos?type=<kind>&target=<who>`) on.
    #[arg(long, default_value = "0.0.0.0:9300")]
    pub metrics_addr: SocketAddr,
    /// Per-phase chaos tolerance (ms): a call is bucketed `chaos="near"` when an
    /// injected fault lands within this of a dialog-state transition (connected/
    /// reinvited/transferred/…) or mid-setup — the "state had no time to propagate,
    /// SIP retransmission recovers it" window = acceptable kill collateral. A call
    /// stably connected across the fault stays `chaos="clear"` (a genuine signal).
    #[arg(long, default_value_t = 200)]
    pub chaos_phase_tolerance_ms: u64,
    #[arg(long, default_value_t = 60)]
    pub options_hold: u64,
    #[arg(long, default_value_t = 5)]
    pub options_cadence: u64,
    /// Realistic ring time (ms): callee dwell between 180 and 200. 0 = immediate.
    #[arg(long, default_value_t = 0)]
    pub ring_delay_ms: u64,
    /// Post-connect talk time (ms) held before BYE on a basic call. 0 = immediate.
    #[arg(long, default_value_t = 0)]
    pub talk_time_ms: u64,
    /// Spacing (ms) held before and after a re-INVITE. 0 = back-to-back.
    #[arg(long, default_value_t = 0)]
    pub reinvite_gap_ms: u64,
    /// Total hold (seconds) of the `long_call` scenario, around its OPTIONS ping.
    #[arg(long, default_value_t = 1200)]
    pub long_hold_secs: u64,
    /// The `X-Api-Call.refer_key` the SUT's REFER backend authorizes — per-run
    /// SUT auth data fed into the refer scenarios' construction (NOT topology;
    /// the transfer target resolves through the egress seam).
    #[arg(long, default_value = "refer-allow-c")]
    pub refer_key: String,
    /// Record roughly 1 call in N's flow (the sampling-gate background fraction).
    /// `1` = full recording (every call); higher values record less. Stored
    /// samples stay bounded by `--sample-cap` per bucket; the cost of full
    /// recording is per-call recording memory, so watch `loadgen_process_*` /
    /// `loadgen_inflight` when it is on.
    #[arg(long, default_value_t = 64)]
    pub background_record_every: u64,
    /// Periodically re-write the on-disk report every N seconds during the run
    /// (so it is browsable mid-run, not only at exit). 0 = only at the end.
    #[arg(long, default_value_t = 0)]
    pub report_interval_secs: u64,
    /// Scenario weights, repeatable: `--scenario basic_call=4 --scenario refer=1`.
    /// A spec may carry per-scenario robustness overrides after the weight:
    /// `--scenario basic_call=4,drop=0.002,retransmit` (drop rate + auto-retransmit
    /// just for that scenario, overriding the global `--drop-rate`/`--auto-retransmit`),
    /// and/or `case=<path.json>` to attach an authored Test case (binding pool →
    /// per-call identities + per-call dwell overrides) to that mix entry,
    /// overriding the global `--case`.
    #[arg(long = "scenario")]
    pub scenarios: Vec<String>,
    /// Global default Test case attached to every mix entry that has no
    /// per-entry `case=` override: an authored `e2e-model` Test-case JSON whose
    /// optional `bindings` pool drives per-call From/To/R-URI (with
    /// `${seq}`/`${seq:N}`/`${rand:N}` expansion), whose recognized extras
    /// (`ring_delay_ms`, `talk_time_ms`, `reinvite_gap_ms`, `long_hold_secs`,
    /// `options_cadence_ms`) override the global dwell flags per call, whose
    /// `checks`/`checkSets` are evaluated over every SAMPLED call's recording
    /// (failing checks reclassify to `check_fail`), and whose
    /// `allowViolations` exempt the named RFC audit rules per call.
    #[arg(long)]
    pub case: Option<PathBuf>,
    /// Directory of shared `e2e-model` Check-set JSONs a Test case may
    /// reference via `checkSets` (the same store the e2e runner reads). A
    /// missing directory is an empty store; a case referencing an unknown set
    /// id fails at startup.
    #[arg(long, default_value = "e2e/checksets")]
    pub check_sets_dir: PathBuf,
    /// Simulated packet-drop probability applied to every call's mux legs (0 =
    /// off). Each datagram (in and out) is independently dropped; the SUT's
    /// transaction layer and, with `--auto-retransmit`, the harness recover it.
    /// Per-scenario override: `--scenario basic_call=4,drop=0.002`.
    #[arg(long, default_value_t = 0.0)]
    pub drop_rate: f64,
    /// Shorthand for `--drop-rate 0.001` (the default 1/1000 loss, so P(3 drops in
    /// a row) ≈ 1e-9). Ignored when `--drop-rate` is set > 0.
    #[arg(long, default_value_t = false)]
    pub drop: bool,
    /// Auto-retransmit lost signaling per real SIP timers (Timer A/E for requests,
    /// 2xx-until-ACK for answers) so a rare drop is recovered instead of failing
    /// the call. Per-scenario override: `--scenario basic_call=4,retransmit`.
    #[arg(long, default_value_t = false)]
    pub auto_retransmit: bool,
    /// The clap ids of the flags that were passed explicitly on the command
    /// line (vs. left at their clap default) — the discriminator for the
    /// flag-overrides-profile precedence. Filled by [`Args::parse`]; a bin
    /// constructing `Args` programmatically inserts the ids of the fields it
    /// wants to override a `--load-profile` value with.
    #[arg(skip)]
    pub explicit: BTreeSet<String>,
}

impl Args {
    /// Parse from the process command line (exiting on error, like
    /// `clap::Parser::parse`), additionally recording each explicitly-passed
    /// flag in [`Args::explicit`]. This inherent method shadows the trait
    /// method on purpose — parsing through the trait would lose the
    /// explicit-flag set and every field would defer to a `--load-profile`.
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    /// [`Args::parse`] over an explicit argv (first item = the program name).
    pub fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        use clap::{CommandFactory, FromArgMatches};
        let matches = Self::command().get_matches_from(itr);
        let mut args = Self::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
        args.explicit = matches
            .ids()
            .filter(|id| {
                matches.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine)
            })
            .map(|id| id.as_str().to_string())
            .collect();
        args
    }

    /// Whether a flag was passed on the command line (vs. left at its clap
    /// default) — the discriminator for the flag-overrides-profile precedence.
    fn explicit(&self, id: &str) -> bool {
        self.explicit.contains(id)
    }
}

/// Resolve the global default [`CallTuning`] from the loss/retransmit flags:
/// `--drop-rate` wins; else `--drop` means the 1/1000 default; else no loss.
fn default_tuning(args: &Args) -> CallTuning {
    let drop_rate = if args.drop_rate > 0.0 {
        args.drop_rate
    } else if args.drop {
        0.001
    } else {
        0.0
    };
    CallTuning { drop_rate, retransmit: args.auto_retransmit }
}

/// The `infraShape` id the flag-synthesized [`EndpointConfig`] carries. A loaded
/// `--endpoint-config` may use any id (loadgen is not a compiled e2e Infra
/// shape) — only the alice/bob/charlie/lb role set is required.
const LOADGEN_SHAPE: &str = "loadgen-mux";

/// Resolve the run's [`EndpointConfig`] — the ONE environment-axis document:
/// the authored file when `--endpoint-config` is given, else an **equivalent**
/// config synthesized from the shorthand flags (`--target` → role `lb`,
/// `--bind-ip`/`--base-port` → alice/bob/charlie binds, the resolved recv
/// timeout, and `--route-pin-to-uas` → the `api-call-pin` egress policy). The
/// `recv_timeout_ms` is the profile-or-flag resolved value (used only on the
/// synthesized path; an authored `--endpoint-config` file carries its own).
fn endpoint_config(args: &Args, recv_timeout_ms: u64) -> EndpointConfig {
    if let Some(path) = &args.endpoint_config {
        let cfg = load_endpoint_config(path)
            .unwrap_or_else(|e| panic!("--endpoint-config {}: {e}", path.display()));
        for role in ["alice", "bob", "charlie", "lb"] {
            assert!(
                cfg.roles.contains_key(role),
                "--endpoint-config {} is missing role {role:?} (loadgen needs alice/bob/charlie binds + the lb ingress)",
                path.display()
            );
        }
        return cfg;
    }
    let roles: std::collections::BTreeMap<String, SocketAddr> = [
        ("alice".to_string(), (args.bind_ip, args.base_port).into()),
        ("bob".to_string(), (args.bind_ip, args.base_port + 1).into()),
        ("charlie".to_string(), (args.bind_ip, args.base_port + 2).into()),
        ("lb".to_string(), args.target),
    ]
    .into();
    EndpointConfig {
        schema: None,
        infra_shape: LOADGEN_SHAPE.to_string(),
        roles,
        recv_timeout_ms,
        transit_delay_ms: 0,
        egress: args
            .route_pin_to_uas
            .then_some(e2e_model::EgressPolicySpec::ApiCallPin),
    }
}

/// Parse one `--scenario` spec
/// (`name[=weight][,drop=<f>][,retransmit[=<bool>]][,case=<path.json>]`)
/// into its resolved [`MixEntry`] (the shape looked up in the unified
/// `registry`, its load body constructed from the per-run `inputs`) plus the
/// per-scenario [`CallTuning`] (starting from `base` and overridden by the
/// trailing tokens); the Test case attached with `case=` is loaded with
/// `case_seed`.
fn parse_scenario_spec(
    spec: &str,
    base: CallTuning,
    registry: &ShapeRegistry,
    inputs: &ScenarioInputs,
    check_sets: &std::collections::BTreeMap<String, e2e_model::CheckSet>,
    case_seed: u64,
) -> (MixEntry, CallTuning) {
    let mut parts = spec.split(',');
    let head = parts.next().unwrap_or(spec);
    let (name, weight) = head
        .split_once('=')
        .map(|(n, w)| (n, w.parse::<f64>().unwrap_or(1.0)))
        .unwrap_or((head, 1.0));
    let mut entry = MixEntry::by_id(registry, name, inputs, weight).unwrap_or_else(|| {
        panic!(
            "unknown load scenario {name:?} (known: {:?})",
            registry
                .iter()
                .filter(|d| d.load.is_some())
                .map(|d| d.id)
                .collect::<Vec<_>>()
        )
    });
    let mut t = base;
    for tok in parts {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        match tok.split_once('=') {
            Some(("drop", v)) => {
                t.drop_rate = v.parse().unwrap_or_else(|_| panic!("bad drop rate in {spec:?}"))
            }
            Some(("retransmit", v)) => {
                t.retransmit =
                    v.parse().unwrap_or_else(|_| panic!("bad retransmit bool in {spec:?}"))
            }
            Some(("case", path)) => {
                entry.case = Some(Arc::new(LoadCase::load(Path::new(path), check_sets, case_seed)));
            }
            None if tok == "retransmit" => t.retransmit = true,
            None if tok == "drop" => {
                if t.drop_rate <= 0.0 {
                    t.drop_rate = 0.001
                }
            }
            _ => panic!("unknown scenario tuning token {tok:?} in {spec:?}"),
        }
    }
    (entry, t)
}

/// Resolve one [`e2e_model::MixSpec`] (a profile mix entry) into its [`MixEntry`]
/// paired with its per-scenario [`CallTuning`], mirroring `parse_scenario_spec`
/// for the CLI scenario string form: the shape is looked up by id, carries its
/// weight and an attached `case`, with per-scenario `dropRate`/`retransmit`
/// overriding `base`.
fn resolve_profile_mix(
    m: &e2e_model::MixSpec,
    base: CallTuning,
    registry: &ShapeRegistry,
    inputs: &ScenarioInputs,
    check_sets: &std::collections::BTreeMap<String, e2e_model::CheckSet>,
    case_seed: u64,
) -> (MixEntry, CallTuning) {
    let mut entry = MixEntry::by_id(registry, &m.shape, inputs, m.weight).unwrap_or_else(|| {
        panic!(
            "load-profile mix references unknown shape {:?} (known: {:?})",
            m.shape,
            registry.iter().filter(|d| d.load.is_some()).map(|d| d.id).collect::<Vec<_>>()
        )
    });
    if let Some(path) = &m.case {
        entry.case = Some(Arc::new(LoadCase::load(path, check_sets, case_seed)));
    }
    let t = CallTuning {
        drop_rate: m.drop_rate.unwrap_or(base.drop_rate),
        retransmit: m.retransmit.unwrap_or(base.retransmit),
    };
    (entry, t)
}

/// Run the complete loadgen application over the given `registry`: the shipped
/// bin is `run(Args::parse(), ShapeRegistry::with_defaults())`; a third-party
/// bin passes the registry it composed instead. The per-run [`ScenarioInputs`]
/// are built from the flags (`--refer-key`); a bin that needs to supply its own
/// uses [`run_with_inputs`].
pub async fn run(args: Args, registry: ShapeRegistry) -> std::io::Result<()> {
    // Per-run scenario inputs (SUT auth data, not topology): consumed at
    // scenario CONSTRUCTION (e.g. the refer scenarios' `refer_key`).
    let inputs = ScenarioInputs { refer_key: args.refer_key.clone() };
    run_with_inputs(args, registry, inputs).await
}

/// [`run`] with caller-supplied per-run [`ScenarioInputs`] (overriding the
/// flag-derived ones) — for a downstream bin whose shape factories consume
/// inputs the upstream CLI has no flag for.
pub async fn run_with_inputs(
    args: Args,
    registry: ShapeRegistry,
    inputs: ScenarioInputs,
) -> std::io::Result<()> {
    // The optional LoadProfile: its fields are DEFAULTS, overridden by any
    // explicitly-passed CLI flag (see `Args::explicit`). Absent = flag-only
    // behaviour, byte-for-byte.
    let profile = args
        .load_profile
        .as_deref()
        .map(|p| e2e_model::load_load_profile(p).unwrap_or_else(|e| panic!("--load-profile {}: {e}", p.display())))
        .unwrap_or_default();

    // Resolve each profile-overridable scalar: explicit flag wins, else profile.
    let cps = if args.explicit("cps") { args.cps } else { profile.cps };
    let duration = if args.explicit("duration") { args.duration } else { profile.duration_secs };
    let max_in_flight =
        if args.explicit("max_in_flight") { args.max_in_flight } else { profile.max_in_flight };
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
    let recv_timeout_ms =
        if args.explicit("recv_timeout_ms") { args.recv_timeout_ms } else { profile.recv_timeout_ms };

    // ONE process-wide monotonic-anchored clock, created here and shared with the
    // mux, every per-call binder, and the chaos log — so all call timelines and
    // the chaos markers ride a single axis (and loadgen reads no raw SystemTime on
    // its own timeline). See `sip_clock::Clock`.
    let clock = Clock::system();

    // Per-run RNG seed off the shared clock (varies per run; no raw SystemTime).
    // Also seeds the Test-case binding resolvers (`${rand:N}` + random pool walk).
    let seed = (clock.now_ms() as u64).max(1);

    // The GLOBAL loss/retransmit default: an explicit loss flag
    // (`--drop-rate`/`--drop`) or `--auto-retransmit` wins; else the profile's
    // `robustness`; else off.
    let base_tuning = {
        let mut t = if args.explicit("drop_rate")
            || args.explicit("drop")
            || args.explicit("auto_retransmit")
        {
            default_tuning(&args)
        } else {
            CallTuning {
                drop_rate: profile.robustness.drop_rate,
                retransmit: profile.robustness.retransmit,
            }
        };
        // A per-flag explicit still overrides the profile individually.
        if args.explicit("drop_rate") || args.explicit("drop") {
            t.drop_rate = default_tuning(&args).drop_rate;
        }
        if args.explicit("auto_retransmit") {
            t.retransmit = args.auto_retransmit;
        }
        t
    };
    // The shared Check-set store a case's `checkSets` resolve against (the
    // same directory the e2e runner reads; missing dir = empty store).
    let check_sets = e2e_model::load_check_sets(&args.check_sets_dir)
        .unwrap_or_else(|e| panic!("--check-sets-dir {}: {e}", args.check_sets_dir.display()));
    // The global default Test case (`--case`): ONE shared resolver — so its
    // `${seq}` counter is monotone across the whole run — attached to every mix
    // entry without its own `case=` override.
    let global_case: Option<Arc<LoadCase>> =
        args.case.as_deref().map(|p| Arc::new(LoadCase::load(p, &check_sets, seed)));
    let mut tuning: std::collections::HashMap<String, CallTuning> = std::collections::HashMap::new();
    let scenarios: Vec<MixEntry> = if !args.scenarios.is_empty() {
        // Explicit `--scenario` set → it wins over the profile's whole mix.
        args.scenarios
            .iter()
            .map(|spec| {
                let (entry, t) =
                    parse_scenario_spec(spec, base_tuning, &registry, &inputs, &check_sets, seed);
                tuning.insert(entry.id.to_string(), t);
                match entry.case.is_some() {
                    true => entry,
                    false => entry.with_case(global_case.clone()),
                }
            })
            .collect()
    } else if !profile.mix.is_empty() {
        // The profile's mix: each entry resolves like a `--scenario` spec (shape id
        // by registry, its own weight, an attached case, and per-scenario
        // loss/retransmit overrides of the global `base_tuning`).
        profile
            .mix
            .iter()
            .map(|m| {
                let (entry, t) = resolve_profile_mix(m, base_tuning, &registry, &inputs, &check_sets, seed);
                tuning.insert(entry.id.to_string(), t);
                match entry.case.is_some() {
                    true => entry,
                    false => entry.with_case(global_case.clone()),
                }
            })
            .collect()
    } else {
        // No explicit scenario set and no profile mix → the default mix; the global
        // tuning applies via `DriverCfg::default_tuning` (no per-id overrides).
        MixEntry::default_mix(&registry, &inputs)
            .into_iter()
            .map(|entry| entry.with_case(global_case.clone()))
            .collect()
    };

    let correlation = match args.correlate.as_str() {
        "header" => Correlation::header_templated(
            args.correlation_header.clone(),
            args.correlation_template.clone(),
            args.correlation_extract.as_deref(),
        )
        .unwrap_or_else(|e| panic!("bad correlation config: {e}")),
        "to-user" | "to_user" => Correlation::to_user(),
        other => panic!("unknown --correlate {other:?} (expected `header` or `to-user`)"),
    };

    // The ONE environment-axis document (authored file or flag-synthesized):
    // endpoint binds + SUT ingress + recv bound + egress policy.
    let endpoint = endpoint_config(&args, recv_timeout_ms);
    let uac = endpoint.addr("alice");
    let uas = endpoint.addr("bob");
    let refer = endpoint.addr("charlie");
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
    };

    // Chaos correlation: the marker log the `POST /chaos` endpoint feeds and the
    // driver classifies each finished call against (near/clear). Shares the run's
    // clock so a marker's wall-clock lands on the same axis as the sampled frames.
    let chaos = Arc::new(
        ChaosLog::new(clock.clone())
            .with_phase_tolerance(Duration::from_millis(args.chaos_phase_tolerance_ms)),
    );

    let driver = Driver::new(cfg, scenarios, reporter.clone(), transport).with_chaos(chaos.clone());
    // The live rate handle (seeded from `--cps` / the profile): `POST /rate`
    // re-targets it and the governor re-anchors its grid; exported as the
    // `loadgen_target_cps` gauge.
    let rate = driver.rate_handle();

    // Live /metrics: reporter series + mux series + chaos markers + the current
    // rate target + a process-memory canary (RSS) so the endurance dashboard can
    // watch the load generator itself while full recording is on.
    let metrics_reporter = reporter.clone();
    let metrics_core = core.clone();
    let metrics_chaos = chaos.clone();
    let metrics_rate = rate.clone();
    let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
        format!(
            "{}{}{}{}{}",
            metrics_reporter.render_prometheus(),
            metrics_core.render_prometheus(),
            metrics_chaos.render_prometheus(),
            target_cps_metric(&metrics_rate),
            process_memory_metrics(),
        )
    });
    let metrics_addr = args.metrics_addr;
    let server_chaos = chaos.clone();
    let server_rate = rate.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(metrics_addr, render, Some(server_chaos), Some(server_rate)).await {
            eprintln!("[loadgen] /metrics server stopped: {e}");
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

    // Periodically snapshot the on-disk report so it is browsable mid-run (the
    // endurance harness copies it out without waiting for the job to finish). Each
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
        "[loadgen] {cps} cps for {duration}s, max_in_flight={max_in_flight}, target={via}, uas={uas}, egress={egress:?}, /metrics on {}",
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
    let orphans = s.orphan_no_header.load(Ordering::Relaxed)
        + s.orphan_unknown_token.load(Ordering::Relaxed)
        + s.orphan_stray.load(Ordering::Relaxed);
    let drops =
        s.dropped_out.load(Ordering::Relaxed) + s.dropped_in.load(Ordering::Relaxed);
    Canaries { orphans, drops, ..Canaries::default() }
}

/// The current offered-rate target as a Prometheus gauge (`loadgen_target_cps`),
/// so the dashboard shows what `POST /rate` last set (and `0` while paused).
fn target_cps_metric(rate: &RateHandle) -> String {
    format!(
        "# HELP loadgen_target_cps Current offered call-rate target (calls/s; 0 = paused).\n\
         # TYPE loadgen_target_cps gauge\n\
         loadgen_target_cps {}\n",
        rate.cps()
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `Args::parse_from` records exactly the explicitly-passed flags — the
    /// discriminator the flag-overrides-profile precedence keys off.
    #[test]
    fn parse_records_explicit_flags() {
        let args = Args::parse_from(["loadgen", "--cps", "30", "--drop"]);
        assert!(args.explicit("cps"));
        assert!(args.explicit("drop"));
        assert!(!args.explicit("duration"));
        assert!(!args.explicit("drop_rate"));
        assert_eq!(args.cps, 30.0);
        assert!(args.drop);
    }
}
