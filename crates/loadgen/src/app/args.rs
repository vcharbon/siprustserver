//! The loadgen command line: its flags, the explicit-flag record behind the
//! flag-overrides-profile precedence, and the run settings resolved from the
//! flags alone (loss tuning, endpoint layout, correlation).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use clap::{ArgMatches, Parser};
use e2e_model::{load_endpoint_config, EndpointConfig, LoadProfile};

use crate::outcome::{LogsTarget, OutcomeSink};
use crate::{CallTuning, Correlation};

/// The `loadgen` CLI surface. Parse with [`Args::parse_from`] or through a
/// [`Product`](super::Product) (NOT `clap::Parser::parse` — the loadgen
/// parsers also record which flags were passed explicitly, the discriminator
/// for the flag-overrides-profile precedence). Fields are `pub` so a product
/// may adjust them ([`Product::configure`](super::Product::configure)); a field
/// set that way only overrides a `--load-profile` value if its clap id is
/// inserted into [`Args::explicit`](field@Args::explicit).
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
    /// The final the SUT refuses a call with under overload (e.g. `503`): that
    /// final to a non-emergency call's initial INVITE before it connects is an
    /// accepted ending (`refused_overload`). Unset, it is a failure like any
    /// final the scenario does not declare.
    #[arg(long)]
    pub overload_reject: Option<u16>,
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
    /// Shorthand: base UDP port: alice/uac=base, bob/uas=base+1, charlie/refer=base+2
    /// (all three = base under `--shared-socket`).
    #[arg(long, default_value_t = 6000)]
    pub base_port: u16,
    /// Shorthand: bind alice, bob and charlie on ONE socket (`base_port`), for a
    /// SUT that sends each new leg back to the ip:port that originated the call.
    /// An `--endpoint-config` giving the roles one address is the explicit form.
    /// Independent of `--correlate`. Ignored when `--endpoint-config` is set.
    #[arg(long, default_value_t = false)]
    pub shared_socket: bool,
    /// Correlation strategy: how the per-call token travels through the SUT.
    /// `header` (default): a transparent header the SUT must RELAY onto every
    /// leg (`--correlation-header`/`--correlation-template`; our b2bua:
    /// `B2BUA_RELAY_HEADERS`). `to-user`: the token IS the To-header user-part —
    /// survives any SIP-correct B2BUA with zero SUT cooperation. `from-user`:
    /// the token IS the From URI user of the attached case's resolved caller,
    /// for a SUT that keeps the calling party's number on every leg; every mix
    /// entry needs a case (checked at startup), and a call whose resolved From
    /// has no user, or whose number a concurrent call holds, is counted
    /// `rejected` before any datagram.
    #[arg(long, default_value = "header")]
    pub correlate: String,
    /// Correlation header name (the `header` strategy): the transparent
    /// per-call token header the SUT relays onto every leg.
    #[arg(long, default_value = "X-Loadgen-Id")]
    pub correlation_header: String,
    /// Correlation header VALUE template with a `${token}` placeholder, so the
    /// token can ride a structured header — e.g. `"${token};encoding=hex"` for
    /// User-to-User, `"icid-value=${token}"` for P-Charging-Vector. Default:
    /// the bare token.
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
    /// The run id every call-outcome record names and every Call-ID carries
    /// (the runner's run; letters, digits, `.`, `_`, `+`, `-`). Default:
    /// `<bin>-<start epoch ms>`.
    #[arg(long)]
    pub run: Option<String>,
    /// The load stream this process runs: every record's `stream`, the
    /// `stream` label of the record counters (`[a-z][a-z0-9_]*`).
    #[arg(long, default_value = "main")]
    pub stream: String,
    /// What every Call-ID carries after the run id, so two generators of one
    /// run (two firings of one stream) never share a Call-ID (letters,
    /// digits, `.`, `_`, `+`, `-`).
    #[arg(long, default_value = "")]
    pub call_id_tag: String,
    /// The JSON-lines file the call-outcome records are appended to. Default:
    /// `<out-dir>/call-outcomes.jsonl`.
    #[arg(long)]
    pub records_file: Option<PathBuf>,
    /// A VictoriaLogs base URL (`http://host:port`) the records are also pushed
    /// to, on `/insert/jsonline` with `_stream_fields=run,source,stream`.
    #[arg(long)]
    pub records_url: Option<String>,
    /// Print the id of every shape with a load body the registry holds, one
    /// per line, and exit without placing a call.
    #[arg(long, default_value_t = false)]
    pub list_shapes: bool,
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
    /// flag-overrides-profile precedence. Filled by the loadgen parsers; a bin
    /// constructing `Args` programmatically inserts the ids of the fields it
    /// wants to override a `--load-profile` value with.
    #[arg(skip)]
    pub explicit: BTreeSet<String>,
}

impl Args {
    /// Parse `itr` (first item = the program name) as the shipped loadgen
    /// command, exiting on error like `clap::Parser::parse`, and record each
    /// explicitly-passed flag in [`Args::explicit`](field@Args::explicit).
    pub fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        use clap::CommandFactory;
        Self::from_matches(&Self::command().get_matches_from(itr)).unwrap_or_else(|e| e.exit())
    }

    /// The args of a parsed command line, with its explicit-flag record.
    pub(super) fn from_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        use clap::FromArgMatches;
        let mut args = Self::from_arg_matches(matches)?;
        args.explicit = matches
            .ids()
            .filter(|id| {
                matches.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine)
            })
            .map(|id| id.as_str().to_string())
            .collect();
        Ok(args)
    }

    /// Whether a flag was passed on the command line (vs. left at its clap
    /// default) — the discriminator for the flag-overrides-profile precedence.
    pub fn explicit(&self, id: &str) -> bool {
        self.explicit.contains(id)
    }
}

/// The GLOBAL loss/retransmit default: an explicit loss flag
/// (`--drop-rate`/`--drop`) or `--auto-retransmit` wins, each over its own
/// field; else the profile's `robustness`; else off.
pub(super) fn base_tuning(args: &Args, profile: &LoadProfile) -> CallTuning {
    let explicit_drop = args.explicit("drop_rate") || args.explicit("drop");
    let explicit_retransmit = args.explicit("auto_retransmit");
    let mut t = if explicit_drop || explicit_retransmit {
        default_tuning(args)
    } else {
        CallTuning {
            drop_rate: profile.robustness.drop_rate,
            retransmit: profile.robustness.retransmit,
            drop_nth: None,
        }
    };
    if explicit_drop {
        t.drop_rate = default_tuning(args).drop_rate;
    }
    if explicit_retransmit {
        t.retransmit = args.auto_retransmit;
    }
    t
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
    CallTuning { drop_rate, retransmit: args.auto_retransmit, drop_nth: None }
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
pub(super) fn endpoint_config(args: &Args, recv_timeout_ms: u64) -> EndpointConfig {
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
    let port =
        |offset: u16| if args.shared_socket { args.base_port } else { args.base_port + offset };
    let roles: std::collections::BTreeMap<String, SocketAddr> = [
        ("alice".to_string(), (args.bind_ip, port(0)).into()),
        ("bob".to_string(), (args.bind_ip, port(1)).into()),
        ("charlie".to_string(), (args.bind_ip, port(2)).into()),
        ("lb".to_string(), args.target),
    ]
    .into();
    EndpointConfig {
        schema: None,
        infra_shape: LOADGEN_SHAPE.to_string(),
        roles,
        recv_timeout_ms,
        transit_delay_ms: 0,
        egress: args.route_pin_to_uas.then_some(e2e_model::EgressPolicySpec::ApiCallPin),
    }
}

/// The run's outcome-record sink: `--run` (else `<source>-<started_ms>`),
/// `--stream`, `--records-file` (else under `--out-dir`), `--records-url`.
pub(super) fn outcome_sink(
    args: &Args,
    source: &str,
    started_ms: i64,
) -> std::io::Result<OutcomeSink> {
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
    let stream = args.stream.as_str();
    let well_formed = stream.starts_with(|c: char| c.is_ascii_lowercase())
        && stream.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !well_formed {
        return Err(invalid(format!("--stream {stream:?}: expected [a-z][a-z0-9_]*")));
    }
    let run = args.run.clone().unwrap_or_else(|| format!("{source}-{started_ms}"));
    let nonce_safe = |s: &str| {
        s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    };
    if run.is_empty() || !nonce_safe(&run) {
        return Err(invalid(format!(
            "--run {run:?}: expected letters, digits, `.`, `_`, `+`, `-`"
        )));
    }
    let tag = args.call_id_tag.as_str();
    if !nonce_safe(tag) {
        return Err(invalid(format!(
            "--call-id-tag {tag:?}: expected letters, digits, `.`, `_`, `+`, `-`"
        )));
    }
    let file =
        args.records_file.clone().unwrap_or_else(|| args.out_dir.join("call-outcomes.jsonl"));
    let logs = args.records_url.as_deref().map(LogsTarget::parse).transpose().map_err(invalid)?;
    Ok(OutcomeSink::new(&run, source, stream, &file, logs)?.with_call_id_tag(tag))
}

/// The run's [`Correlation`] from `--correlate` and the header knobs.
pub(super) fn correlation(args: &Args) -> Result<Correlation, String> {
    match args.correlate.as_str() {
        "header" => Correlation::header_templated(
            args.correlation_header.clone(),
            args.correlation_template.clone(),
            args.correlation_extract.as_deref(),
        )
        .map_err(|e| format!("bad correlation config: {e}")),
        "to-user" | "to_user" => Ok(Correlation::to_user()),
        "from-user" | "from_user" => Ok(Correlation::from_user()),
        other => Err(format!(
            "unknown --correlate {other:?} (expected `header`, `to-user` or `from-user`)"
        )),
    }
}
