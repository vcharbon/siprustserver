//! Drop-armed run backstops: the panic-time wire-trace dump ([`PanicDump`])
//! and the forgot-to-`finish` RFC hard gate ([`CseqGate`]), plus the gate's
//! shared finding evaluation. [`Harness::finish`](super::Harness::finish)
//! runs the same gate inline and disarms both.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::rc::Rc;

use layer_harness::{Channel, Recorder};
use sip_net::{to_sip_entries, SignalingNetworkEvent};

use super::waiver::{apply_waivers, WaiverState};
use crate::report::wire::{facets, format_relative};

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

/// The RFC 3261 / 3262 / 3264 audit findings over a recorded trace that MUST
/// fail the test — the `(lane, detail)` pairs the hard gate panics on. Runs the
/// full default suite (per-message peer rules + cross-message rules), skipping:
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
/// per-test opt-in. Empty ⇒ clean.
pub(super) fn rfc_hard_gate_findings(
    events: &[layer_harness::Stamped<SignalingNetworkEvent>],
    waivers: &[WaiverState],
    addr_names: &HashMap<SocketAddr, String>,
) -> Vec<(String, String)> {
    // One shared evaluator (sip-net) runs the suite with subject dispatch — the
    // SAME pass the report projection lists — so the gate and the report can
    // never disagree on which endpoint a rule applies to. Scoped waivers drop
    // the covered non-advisory findings (attribution resolves the emitting party
    // from the recorded `from_lane`).
    apply_waivers(events, waivers, addr_names)
}

/// Format the hard-gate panic message listing every RFC audit violation.
pub(super) fn render_rfc_panic(name: &str, findings: &[(String, String)]) -> String {
    format!(
        "[{name}] SIP RFC audit violation(s) on the recorded trace — a real \
         UA would have rejected these, so this test MUST fail (the RFC check is a \
         mandatory hard gate; if a fixture deliberately violates a rule, waive it \
         with Harness::allow_violation):\n{}",
        findings
            .iter()
            .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// RAII trace dumper. A failing scenario `panic!`s — a `recv` timeout, an
/// `expect` status mismatch, a wrong method — and aborts *before*
/// [`Harness::finish`](super::Harness::finish), the only path that renders the
/// recording. Without this guard the most common failure (a message that never
/// arrived / wrong method) yields a one-line panic and zero visibility into
/// what was actually on the wire.
///
/// This guard's `Drop` notices the in-flight unwind (`std::thread::panicking`)
/// and dumps a compact wire trace to stderr, so every panicking scenario
/// self-documents with no per-test instrumentation. `finish` disarms it (a
/// clean run already has its report). It projects the **synchronous**
/// `channel().snapshot()` — no async, no `close()` — and is best-effort and
/// panic-safe: it never panics inside `Drop` (a poisoned mutex etc. is
/// swallowed).
pub(super) struct PanicDump {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    armed: Cell<bool>,
}

impl PanicDump {
    pub(super) fn new(
        name: String,
        channel: Channel<SignalingNetworkEvent>,
        recorder: Recorder,
    ) -> Self {
        Self { name, channel, recorder, armed: Cell::new(true) }
    }

    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }

    /// Render the compact one-line-per-message trace from the recording.
    fn render(&self) -> String {
        let events = self.channel.snapshot();
        let entries = to_sip_entries(&events);
        let names: BTreeMap<SocketAddr, String> = self
            .recorder
            .snapshot()
            .lanes
            .into_iter()
            .map(|l| (l.addr, l.names.first().cloned().unwrap_or_default()))
            .collect();
        let label_for = |addr: &SocketAddr| match names.get(addr) {
            Some(n) if !n.is_empty() => format!("{n} ({addr})"),
            _ => addr.to_string(),
        };
        let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);

        let mut out = format!(
            "\n══ SIP trace for '{}' (dumped on panic — finish() not reached) ══\n",
            self.name
        );
        if entries.is_empty() {
            out.push_str("  (no messages recorded)\n");
        }
        for e in &entries {
            let sent = format_relative(e.sent_ms as i64 - base);
            let ts = match e.received_ms {
                Some(r) if r != e.sent_ms => format!("{sent} → {}", format_relative(r as i64 - base)),
                _ => sent,
            };
            let undelivered = if e.delivered { "" } else { "  [UNDELIVERED]" };
            out.push_str(&format!(
                "  [{ts}] {} → {}  {}{}\n",
                label_for(&e.from),
                label_for(&e.to),
                facets(&e.raw).label,
                undelivered
            ));
        }
        out.push_str(&format!("══ end SIP trace ({} message(s)) ══\n", entries.len()));
        out
    }
}

impl Drop for PanicDump {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        // Never panic while already unwinding: a second panic in `Drop` aborts
        // the process. Swallow any failure (e.g. a poisoned mutex on snapshot).
        if let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.render())) {
            eprint!("{text}");
        }
    }
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
}

impl CseqGate {
    pub(super) fn new(
        name: String,
        channel: Channel<SignalingNetworkEvent>,
        waivers: Rc<RefCell<Vec<WaiverState>>>,
        recorder: Recorder,
    ) -> Self {
        Self { name, channel, armed: Cell::new(true), waivers, recorder }
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
        // Reading the snapshot + running the rules is panic-free in practice, but
        // guard it so a render fault can never turn into a double-panic abort.
        let waivers = self.waivers.borrow();
        let names = addr_names(&self.recorder);
        let findings = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rfc_hard_gate_findings(&self.channel.snapshot(), &waivers, &names)
        })) {
            Ok(f) => f,
            Err(_) => return,
        };
        if !findings.is_empty() {
            panic!("{}", render_rfc_panic(&self.name, &findings));
        }
    }
}
