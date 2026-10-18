//! The RFC audit hard gate: the findings a run fails on, their panic text, and
//! the forgot-to-`finish` Drop backstop ([`CseqGate`]).
//! [`Harness::finish`](super::Harness::finish) runs the same gate inline and
//! disarms the backstop.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::Rc;

use layer_harness::{Channel, NetworkTag, Recorder};
use sip_net_testkit::{audit_wire_entries, RfcFinding, SignalingNetworkEvent};

use super::dialog_gate::{render_dialog_panic, surviving_verdicts};
use super::failure_dump::Judges;
use super::waiver::{apply_waivers, WaiverState};
use crate::report::wire::facets;

/// The recorder's addr → party-name map (first name per bind) — resolves the
/// `from_lane` bind key on a recorded entry to the emitting party for waiver
/// attribution.
pub(super) fn addr_names(recorder: &Recorder) -> HashMap<SocketAddr, String> {
    recorder
        .snapshot()
        .lanes
        .into_iter()
        .map(|l| (l.addr, l.names.first().cloned().unwrap_or_default()))
        .collect()
}

/// The names of the lanes the harness bound as SUT (`NetworkTag::Core`).
fn sut_names(recorder: &Recorder) -> HashSet<String> {
    recorder
        .snapshot()
        .lanes
        .into_iter()
        .filter(|l| l.network == NetworkTag::Core)
        .flat_map(|l| l.names)
        .collect()
}

/// The audit gate's outcome: the findings the run fails on, and per rule the
/// findings charged to the SUT that a waiver naming it covered.
pub(super) struct HardGate {
    pub gating: Vec<GatedFinding>,
    pub waived_sut: BTreeMap<String, usize>,
    /// Per rule, the findings charged to a simulated party a waiver covered.
    pub waived_simulated: BTreeMap<String, usize>,
}

/// `waived SUT findings: rule×n, …`, or `None` when no SUT finding was waived.
pub(super) fn waived_sut_line(waived: &BTreeMap<String, usize>) -> Option<String> {
    if waived.is_empty() {
        return None;
    }
    let rules: Vec<String> = waived.iter().map(|(rule, n)| format!("{rule}×{n}")).collect();
    Some(format!("waived SUT findings: {}", rules.join(", ")))
}

/// One audit finding the gate fails the run on: the rule, the party it
/// charges, and the offending message (its 1-based audit position and its
/// ladder label) when the rule pinpoints one.
pub(super) struct GatedFinding {
    pub finding: RfcFinding,
    pub party: Option<String>,
    /// `sender → receiver  <first line>` of the offending message.
    pub message: Option<String>,
}

impl GatedFinding {
    /// `rule <id> fired (charged to <party>) at message #<n> (<label>)`.
    fn head(&self) -> String {
        let party = self.party.as_deref().unwrap_or("no party");
        let at = match (self.finding.offending, &self.message) {
            (Some(n), Some(label)) => format!(" at message #{n} ({label})"),
            (Some(n), None) => format!(" at message #{n}"),
            (None, _) => String::new(),
        };
        format!("rule {} fired (charged to {party}){at}", self.finding.rule)
    }
}

/// The RFC 3261 / 3262 / 3264 audit findings over a recorded trace that MUST
/// fail the test. Runs over the full default suite (per-message peer rules +
/// cross-message rules), skipping:
///   - any `force_advisory()` rule (architectural divergences recorded but not
///     gated — they still reach the report via the layer-close `close()`);
///   - any finding whose rule `subject()` does not intersect the originating
///     bind's declared roles (default = all roles, so this only narrows when a
///     test sets roles);
///   - any finding a test waived via a scoped
///     [`WaiverScope`](super::WaiverScope) (attributed to the emitting party via
///     the recorder's `from_lane`), including the coarse
///     [`Harness::allow_violation`](super::Harness::allow_violation).
///
/// ONLY the audit rules run here (the structural layer-close anomalies —
/// in-flight imbalance, queue leaks — are deliberately not consulted), so
/// timeout / reap / stall fixtures are not gated. Shared by `Harness::finish`
/// and the `Harness` Drop guard so the SAME suite runs on every run with no
/// per-test opt-in. Empty `gating` ⇒ clean.
pub(super) fn rfc_hard_gate_findings(
    findings: &[RfcFinding],
    events: &[layer_harness::Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    recorder: &Recorder,
) -> HardGate {
    // `findings` is the output of the ONE shared evaluator
    // (`sip_net_testkit::evaluate_rfc_findings`, subject dispatch applied) — the SAME
    // set the report projection lists — so the gate and the report can never
    // disagree on which endpoint a rule applies to.
    let names = addr_names(recorder);
    let entries = audit_wire_entries(events);
    let name_of = |addr: &SocketAddr| match names.get(addr) {
        Some(n) if !n.is_empty() => n.clone(),
        _ => addr.to_string(),
    };
    let waived = apply_waivers(findings, events, waivers, &names, &sut_names(recorder));
    let gating = waived
        .gating
        .into_iter()
        .map(|(finding, party)| {
            let message = finding.offending.and_then(|n| entries.get(n.checked_sub(1)?)).map(|e| {
                format!("{} → {}  {}", name_of(&e.from), name_of(&e.to), facets(&e.raw).label)
            });
            GatedFinding { finding, party, message }
        })
        .collect();
    HardGate { gating, waived_sut: waived.sut, waived_simulated: waived.simulated }
}

/// The hard-gate panic text: a first line naming the first rule that fired
/// and the party it charges, then one line per finding.
pub(super) fn render_rfc_panic(name: &str, findings: &[GatedFinding]) -> String {
    let more = match findings.len() {
        0 | 1 => String::new(),
        n => format!(" (and {} more finding(s))", n - 1),
    };
    let first = findings.first().map(GatedFinding::head).unwrap_or_default();
    format!(
        "red because RFC audit {first}{more}\n[{name}] the recorded trace violates the SIP RFC \
         audit, a mandatory hard gate (a fixture that deliberately violates a rule waives it \
         with Harness::waive(WaiverScope::rule(..).on_party(..))):\n{}",
        findings
            .iter()
            .map(|g| format!("  • {} [{}]: {}", g.head(), g.finding.lane, g.finding.detail))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// RAII backstop for the RFC 3261 CSeq hard gate when a
/// [`Harness`](super::Harness) is dropped WITHOUT `finish`. `finish` runs the
/// gate inline and disarms this; a harness left to drop (or whose scenario
/// forgot to `finish`) still gets the same mandatory check. On Drop, if still
/// armed and the test is not already unwinding, it computes the cross-message
/// (cseq) findings over the recorded channel and `panic!`s on any — failing the
/// test. Only the cseq rules run here (the structural layer-close anomalies are
/// NOT consulted), so timeout / reap / stall fixtures are not gated. The
/// `!std::thread::panicking()` guard prevents a double-panic when the test is
/// already failing.
pub(super) struct CseqGate {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    armed: Cell<bool>,
    /// Shared with the owning `Harness` so scoped waivers registered before
    /// `finish`/Drop are honoured by this Drop backstop too.
    waivers: Rc<RefCell<Vec<WaiverState>>>,
    /// Resolves the emitting party from a recorded `from_lane` for waiver
    /// attribution (same source the `Harness` uses at `finish`).
    recorder: Recorder,
    /// The dialog judges of the harness's UAs: their verdicts gate here too.
    judges: Judges,
}

impl CseqGate {
    pub(super) fn new(
        name: String,
        channel: Channel<SignalingNetworkEvent>,
        waivers: Rc<RefCell<Vec<WaiverState>>>,
        recorder: Recorder,
        judges: Judges,
    ) -> Self {
        Self { name, channel, armed: Cell::new(true), waivers, recorder, judges }
    }

    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for CseqGate {
    fn drop(&mut self) {
        if !self.armed.get() || std::thread::panicking() {
            return;
        }
        let verdicts = surviving_verdicts(&self.judges.borrow(), &self.waivers.borrow()).0;
        if !verdicts.is_empty() {
            panic!("{}", render_dialog_panic(&self.name, &verdicts));
        }
        // Reading the snapshot + running the rules is panic-free in practice, but
        // guard it so a render fault can never turn into a double-panic abort.
        let findings = {
            let waivers = self.waivers.borrow();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let events = self.channel.snapshot();
                let suite = sip_net_testkit::evaluate_rfc_findings(&events);
                rfc_hard_gate_findings(&suite, &events, &waivers, &self.recorder).gating
            })) {
                Ok(f) => f,
                Err(_) => return,
            }
        };
        if !findings.is_empty() {
            panic!("{}", render_rfc_panic(&self.name, &findings));
        }
    }
}
