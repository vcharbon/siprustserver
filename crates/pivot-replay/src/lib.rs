//! The replay runner over the pivot interpreter: a run-spec in, a run bundle
//! out, the exit code the structural verdict. The run sequence itself (compile,
//! arm the bundle writer, drive, commit) is `pivot_interpreter::replay`'s; this
//! library holds what a lane assembles around it (the case's load and lint
//! gate, the numbers, the agents, the post-run RFC audit, the bundle's side
//! files, the verdict's verification lines), the simulated lane, the hosted
//! lane and the batch process that runs it, and the registration seam a
//! product fills ([`register`], ADR-0060 appendix §13). The
//! `pivot-replay` binary runs it with nothing registered.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use pivot_schema::bundle::{ClockMode, RunConfig, RunRfcAudit};
use pivot_schema::PivotV3;

pub mod attachments;
pub mod audit;
pub mod batch;
pub mod cancellation;
pub mod cap_readings;
pub mod compose;
pub mod diag;
pub mod exports;
pub mod hosted;
pub mod http_stub;
pub mod lanes;
pub mod nofile;
pub mod numbers;
pub mod plain;
pub mod reap;
pub mod register;
pub mod simulated;
pub mod spec;
pub mod sut;
pub mod sut_join;
pub mod tail;
pub mod terminated;
pub mod verdict;
pub mod verifications;

/// Runs one spec of the binary with nothing registered: its case on the
/// simulated lane against the plain system ([`plain::PlainB2bua`]).
pub fn execute(spec: &spec::RunSpec) -> Result<RunSummary, Failure> {
    execute_on(spec, &plain::PlainB2bua::default())
}

/// Runs one spec of the binary's run-spec shape: its case on the simulated
/// lane against `sut`.
pub fn execute_on(
    spec: &spec::RunSpec,
    sut: &dyn register::SimulatedSut,
) -> Result<RunSummary, Failure> {
    let spec::LaneSpec::Simulated(lane) = &spec.lane;
    let case = Case::load(&spec.case, false, &spec.run.endpoint_addresses)?;
    let lane = simulated::SimulatedLane {
        out_dir: &spec.out_dir,
        overlay: &spec.run,
        egress_endpoint: &lane.egress_endpoint,
        sdp: lane.sdp,
        rtp_base: lane.rtp_base,
    };
    simulated::run(&lane, &case, sut)
}

/// Why a run produced no verdict, ordered by exit code (see [`exit_code`]).
#[derive(Debug)]
pub enum Failure {
    /// The document or the spec refuses this run: a lint error on a strict
    /// lane, a document that does not parse or compile, an endpoint the lane
    /// cannot place. Exit 2.
    Refused(String),
    /// The host failed the run: an unreadable file, a spawn or bind error, a
    /// bundle that could not be written. Exit 3.
    Environment(String),
    /// The run body panicked. The armed bundle writer still left the ladder
    /// under a `RunUnwound` verdict. Exit 4.
    Panicked(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(m) => write!(f, "refused: {m}"),
            Failure::Environment(m) => write!(f, "environment: {m}"),
            Failure::Panicked(m) => write!(f, "panicked: {m}"),
        }
    }
}

/// One finished run, as the caller judges it.
pub struct RunSummary {
    pub verdict: pivot_schema::bundle::VerdictStatus,
    pub failures: usize,
    /// The lane's own post-run cleanup gate: `Err` holds what the system under
    /// test still held. Never disabled: where the system is shared by a whole
    /// batch the gate is the batch's, because a gauge counting every cell in
    /// flight cannot answer for one call.
    pub cleanup: Result<(), String>,
    /// The post-run RFC audit over the lane's recorded wire (`rfc.json`). A
    /// gating finding fails the run: the scripted peers answer whatever they
    /// are handed, so this is the one instrument that sees a violation of the
    /// system under test they tolerate. A source fault is written for
    /// information and fails nothing.
    pub rfc: RunRfcAudit,
    pub bundle: PathBuf,
}

impl RunSummary {
    /// The run passed: green verdict, clean post-run state, no gating RFC
    /// finding.
    pub fn passed(&self) -> bool {
        use pivot_schema::bundle::VerdictStatus;
        matches!(self.verdict, VerdictStatus::Ok | VerdictStatus::OkNegative)
            && self.cleanup.is_ok()
            && self.rfc.passed()
    }
}

/// The process exit vocabulary: 0 pass, 1 failed run (verdict, cleanup or RFC
/// audit), 2 refused, 3 environment, 4 panicked.
pub fn exit_code(result: &Result<RunSummary, Failure>) -> i32 {
    match result {
        Ok(summary) if summary.passed() => 0,
        Ok(_) => 1,
        Err(Failure::Refused(_)) => 2,
        Err(Failure::Environment(_)) => 3,
        Err(Failure::Panicked(_)) => 4,
    }
}

/// The one line a finished run is summarised in, `label` naming it.
pub fn summary_line(label: &str, summary: &RunSummary) -> String {
    let cleanup = match &summary.cleanup {
        Ok(()) => "clean".to_string(),
        Err(held) => format!("DIRTY ({held})"),
    };
    let rfc = match &summary.rfc {
        RunRfcAudit::NotAudited { reason } => format!("rfc not audited ({reason})"),
        RunRfcAudit::Audited { findings } => {
            let relayed = findings.iter().filter(|f| f.caused_by.is_some()).count();
            let relayed = if relayed == 0 {
                String::new()
            } else {
                format!(" ({relayed} relayed from another party)")
            };
            let (gating, source) = (summary.rfc.gating(), summary.rfc.source_faults());
            format!(
                "{gating} gating / {source} source fault / {} other rfc finding(s){relayed}",
                findings.len() - gating - source
            )
        }
    };
    format!(
        "{label}: {verdict:?} ({failures} failure(s)), sut {cleanup}, {rfc} — bundle {bundle}",
        verdict = summary.verdict,
        failures = summary.failures,
        bundle = summary.bundle.display(),
    )
}

/// The loaded, gated, address-resolved case every lane starts from.
pub struct Case {
    pub document: PivotV3,
    pub plan: pivot_interpreter::Plan,
    pub base_dir: PathBuf,
    /// Endpoint id → the socket the lane binds it at (the run-spec override,
    /// else the document's `observed`).
    pub endpoint_addrs: BTreeMap<String, SocketAddr>,
}

impl Case {
    /// The case at `case` (a document, or a directory holding `scenario.json`),
    /// through the lint gate (`strict` refuses on a lint error), parsed,
    /// compiled, and each endpoint placed at its `endpoint_addresses` override
    /// or its observed address.
    pub fn load(
        case: &Path,
        strict: bool,
        endpoint_addresses: &BTreeMap<String, String>,
    ) -> Result<Self, Failure> {
        let path = if case.is_dir() { case.join("scenario.json") } else { case.to_path_buf() };
        let base_dir = path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| Failure::Environment(format!("{} has no parent dir", path.display())))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Failure::Environment(format!("read {}: {e}", path.display())))?;
        lint_gate(&text, strict)?;
        let document = PivotV3::from_json(&text)
            .map_err(|e| Failure::Refused(format!("{} does not parse: {e}", path.display())))?;
        let plan = pivot_interpreter::Plan::compile(document.clone()).map_err(|errors| {
            let list: Vec<String> = errors.iter().map(|e| format!("  {e}")).collect();
            Failure::Refused(format!("the document does not compile:\n{}", list.join("\n")))
        })?;
        let mut endpoint_addrs = BTreeMap::new();
        for endpoint in &document.endpoints {
            let stated = endpoint_addresses
                .get(&endpoint.id)
                .cloned()
                .unwrap_or_else(|| endpoint.observed.clone());
            let addr: SocketAddr = stated.parse().map_err(|e| {
                Failure::Refused(format!(
                    "endpoint {:?} address {stated:?} is not a socket this lane can bind ({e}); \
                     state run.endpoint_addresses[{:?}]",
                    endpoint.id, endpoint.id
                ))
            })?;
            endpoint_addrs.insert(endpoint.id.clone(), addr);
        }
        Ok(Case { document, plan, base_dir, endpoint_addrs })
    }

    /// The socket the lane's egress directive targets, by endpoint id, at the
    /// addresses the document states.
    pub fn egress_addr(&self, endpoint: &str) -> Result<SocketAddr, Failure> {
        egress_target(&self.endpoint_addrs, endpoint, self.no_dial())
    }

    /// Why the document dials nobody, where it dials nobody at all.
    pub fn no_dial(&self) -> Option<NoDial> {
        NoDial::of(&self.document.calls)
    }
}

/// Why a document dials nobody, which is what its egress directive is pointed
/// by: the lane has no callee endpoint to name and must still hand the system
/// under test a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDial {
    /// The routing decision REFUSED the call, so nothing is ever launched at
    /// the target. The document's own first endpoint stands in: a route the
    /// system makes anyway then lands on a bound socket and is recorded as the
    /// divergence it is.
    Refused,
    /// The CALLER abandoned before any called leg crossed the vantage. Nothing
    /// refused the dial, so the system DOES launch one, and it must reach
    /// nobody: the b-leg the capture never held is then neither scripted nor
    /// asserted, where a bound socket would record it against a party the
    /// document plays.
    Abandoned,
}

impl NoDial {
    /// Why these calls dial nobody, or `None` where one of them dials.
    pub fn of(calls: &[pivot_schema::call::Call]) -> Option<NoDial> {
        if !calls.iter().all(|call| call.attempts.is_empty()) {
            return None;
        }
        if calls.iter().any(|call| call.abandoned.is_some()) {
            Some(NoDial::Abandoned)
        } else {
            Some(NoDial::Refused)
        }
    }
}

/// The port an egress that must reach nobody targets: the discard service, on
/// no lane's number plan and below every ephemeral range an endpoint binds in.
pub const UNBOUND_EGRESS_PORT: u16 = 9;

/// `addr`'s host at [`UNBOUND_EGRESS_PORT`]: reachable, so the system's dial
/// leaves as it did in the capture, and bound by nobody, so it arrives nowhere.
pub fn unbound_egress(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip(), UNBOUND_EGRESS_PORT)
}

/// The socket the lane's egress directive targets, over the addresses the lane
/// holds for the document's endpoints.
///
/// An ABANDONED case takes [`unbound_egress`] whatever the directive names: the
/// system still dials, and every socket the lane holds belongs to a party the
/// document plays. A document that dials nobody for any other reason names no
/// callee endpoint either, and its own first endpoint stands in.
pub fn egress_target(
    addrs: &BTreeMap<String, SocketAddr>,
    endpoint: &str,
    no_dial: Option<NoDial>,
) -> Result<SocketAddr, Failure> {
    if no_dial == Some(NoDial::Abandoned) {
        if let Some(addr) = addrs.values().next() {
            return Ok(unbound_egress(*addr));
        }
    }
    if let Some(addr) = addrs.get(endpoint) {
        return Ok(*addr);
    }
    if no_dial.is_some() {
        if let Some(addr) = addrs.values().next() {
            return Ok(*addr);
        }
    }
    Err(Failure::Refused(format!("egress_endpoint {endpoint:?} names no endpoint in the document")))
}

/// The v3 lint gate, before any wire traffic: errors refuse a strict lane; a
/// lane that is not strict reports everything as advisory and proceeds.
pub fn lint_gate(text: &str, strict: bool) -> Result<(), Failure> {
    let report = pivot_schema::lint::lint_str(text);
    if report.diagnostics.is_empty() {
        return Ok(());
    }
    if strict && report.has_errors() {
        return Err(Failure::Refused(format!("lint refuses this lane:\n{}", report.render())));
    }
    eprint!("{}", report.render());
    Ok(())
}

/// The compiled run configuration a lane hands the interpreter: the lane's own
/// facts (name, clock, ingress) plus the spec's §4.3 overlay, verbatim.
pub fn run_config(
    lane_name: &str,
    clock: ClockMode,
    route_target: String,
    overlay: &spec::RunOverlay,
    identities: pivot_schema::bundle::IdentityBindings,
) -> RunConfig {
    RunConfig {
        lane: lane_name.to_string(),
        clock,
        injected_headers: overlay.injected_headers.clone(),
        call_headers: BTreeMap::new(),
        timing_tolerance_ms: overlay.timing_tolerance_ms,
        route_target,
        identities,
        claim_numbers: overlay.claim_numbers.clone(),
        endpoint_addresses: overlay.endpoint_addresses.clone(),
        check_scoping: overlay.check_scoping.clone(),
        known_bugs: overlay.known_bugs.clone(),
        media: pivot_schema::bundle::MediaMode::default(),
        identity_nonce: None,
    }
}

#[cfg(test)]
mod tests;
