//! **RFC-audit acceptance scoping** for the failover harness: which recorded
//! deviations gate the run and which are declared-accepted.
//!
//! Two scopes, coarse to fine:
//! - a **lifetime waiver** — the rule is not judged at all on this run (no
//!   baseline left in the recording);
//! - an **acceptance window** — the rule is accepted only on messages captured
//!   inside the window (armed at a fault injection), and gates everywhere else,
//!   so establishment and every fault-free scenario still judge it in full.
//!
//! A window is anchored on the recording's **capture order** (`seq`), not on a
//! timestamp: under a paused clock many messages share one `at_ms`/`sent_ms`, so
//! only `seq` can say "before the arm" exactly (`layer_harness::EventSequencer`).
//!
//! A finding is placed in a window through its `RfcFinding::offending` wire-entry
//! index; a finding the rule cannot pin to an entry is unattributable and gates.

use std::collections::{HashMap, HashSet};

use layer_harness::Stamped;
use sip_message::Method;
use sip_net_testkit::{CaptureIndex, DialogKey, SignalingNetworkEvent};

/// One RFC-audit rule accepted over a bounded slice of the run, in capture order.
struct AcceptanceWindow {
    /// The audit rule `name()` accepted inside the window.
    rule: String,
    /// Recording `seq` of the last event captured BEFORE the window opened, so
    /// the window covers strictly-later captures only.
    from_seq: u64,
    /// Recording `seq` the window closed at; `None` ⇒ open to the end of the run.
    until_seq: Option<u64>,
    /// At most one offending request per dialog is accepted when set; a
    /// second request of a dialog the window already accepted one of gates.
    once_per_dialog: bool,
    /// What the window admits of an offending message beyond its rule and
    /// span; `None` admits every one.
    admits: Option<Admits>,
}

/// A window's test of an offending message: the message's index in the
/// capture, the capture, and the capture `seq` the window opened after (an
/// entry at or below it was captured before the window).
pub type Admits = fn(usize, &CaptureIndex, u64) -> bool;

impl AcceptanceWindow {
    /// Whether the message captured at `seq` falls inside this window.
    fn covers(&self, seq: u64) -> bool {
        seq > self.from_seq && self.until_seq.is_none_or(|until| seq <= until)
    }

    /// Whether the window takes the offending message at index `i` of
    /// `capture`: inside its span and admitted by its test.
    fn takes(&self, i: usize, capture: &CaptureIndex) -> bool {
        let Some(entry) = capture.entry(i) else { return false };
        self.covers(entry.seq) && self.admits.is_none_or(|admits| admits(i, capture, self.from_seq))
    }
}

/// One audit finding as the failover gate carries it: `(rule, lane, detail)`.
pub type Finding = (String, String, String);

/// `(rule, lane, detail)` findings reduced to the `(lane, detail)` pairs the
/// assertion messages render.
pub fn lane_details(findings: Vec<Finding>) -> Vec<(String, String)> {
    findings.into_iter().map(|(_, lane, detail)| (lane, detail)).collect()
}

/// The declared RFC-audit scoping of one harness run: the lifetime waivers plus
/// the acceptance windows, and the classification they imply.
#[derive(Default)]
pub struct RfcAcceptance {
    /// Rule `name()`s waived for the whole run.
    waived: HashSet<String>,
    /// Window-scoped acceptances; outside every window the rule gates in full.
    windows: Vec<AcceptanceWindow>,
}

impl RfcAcceptance {
    /// Stop judging `rule` for the rest of the run (the coarse scope).
    pub fn waive_lifetime(&mut self, rule: &str) {
        self.waived.insert(rule.to_string());
    }

    /// Open an acceptance window for `rule` covering every message captured
    /// AFTER `from_seq` (the recording's high-water mark at the arm instant).
    pub fn open_window(&mut self, rule: &str, from_seq: u64) {
        let window = AcceptanceWindow {
            rule: rule.to_string(),
            from_seq,
            until_seq: None,
            once_per_dialog: false,
            admits: None,
        };
        self.windows.push(window);
    }

    /// [`open_window`](Self::open_window), accepting at most one offending
    /// request per dialog: the request is named by its dialog (Call-ID and
    /// tags), method and CSeq, so its copies on every hop count once.
    pub fn open_window_once_per_dialog(&mut self, rule: &str, from_seq: u64) {
        self.open_window_where(rule, from_seq, true, None);
    }

    /// An acceptance window for `rule` over every message captured after
    /// `from_seq`, one request per dialog when `once_per_dialog`, accepting
    /// only the offending messages `admits` passes when it is stated.
    pub fn open_window_where(
        &mut self,
        rule: &str,
        from_seq: u64,
        once_per_dialog: bool,
        admits: Option<Admits>,
    ) {
        let window = AcceptanceWindow {
            rule: rule.to_string(),
            from_seq,
            until_seq: None,
            once_per_dialog,
            admits,
        };
        self.windows.push(window);
    }

    /// Close EVERY open window for `rule` at `until_seq`, so the rule gates in
    /// full from there on even when several windows were armed.
    pub fn close_windows(&mut self, rule: &str, until_seq: u64) {
        for w in self.windows.iter_mut().filter(|w| w.rule == rule && w.until_seq.is_none()) {
            w.until_seq = Some(until_seq);
        }
    }

    /// Whether `rule` carries a lifetime waiver.
    pub fn waived(&self, rule: &str) -> bool {
        self.waived.contains(rule)
    }

    /// Whether an acceptance window for `rule` covers the finding whose offending
    /// message is the 1-based `offending` index into `capture`. A finding with no
    /// pinned message is never accepted — it cannot be placed in the run.
    pub fn accepts(&self, rule: &str, offending: Option<usize>, capture: &CaptureIndex) -> bool {
        let Some(i) = offending.and_then(|i| i.checked_sub(1)) else { return false };
        self.windows.iter().any(|w| w.rule == rule && w.takes(i, capture))
    }

    /// Whether a window for `rule` taking the finding at `offending` is one
    /// that accepts one request per dialog (`once_per_dialog`), or one that
    /// accepts every request it takes (`!once_per_dialog`).
    fn taken_by(
        &self,
        rule: &str,
        offending: Option<usize>,
        capture: &CaptureIndex,
        once_per_dialog: bool,
    ) -> bool {
        offending.and_then(|i| i.checked_sub(1)).is_some_and(|i| {
            self.windows.iter().any(|w| {
                w.rule == rule && w.once_per_dialog == once_per_dialog && w.takes(i, capture)
            })
        })
    }

    /// Run the cross-message audit over `events` (already the audit-visible,
    /// endpoint-scoped view, the binds of `scoped_out` taken out of it) and
    /// split its non-advisory findings into `(gating, accepted)`: a finding is
    /// accepted when its OFFENDING message was captured inside a window for its
    /// rule; a lifetime-waived rule is dropped outright; an unattributable
    /// finding gates.
    pub fn partition(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
        scoped_out: &HashSet<String>,
    ) -> (Vec<Finding>, Vec<Finding>) {
        let capture = CaptureIndex::new(sip_net_testkit::audit_wire_entries(events));
        let mut gating = Vec::new();
        let mut accepted = Vec::new();
        for rule in sip_net_testkit::rfc_cross_message_rules_scoped(scoped_out.clone()) {
            // Honour the advisory tier exactly as the scenario-harness hard gate
            // does: a `force_advisory` rule (a documented B2BUA-architectural
            // divergence — per-leg SDP re-origin, OPTIONS-keepalive response
            // headers, the un-timeable proxy-100 bound, …) is recorded, not
            // gated. Skipping it here keeps the failover matrix from failing on
            // the same architectural divergences the main gate already excuses.
            if rule.force_advisory() {
                continue;
            }
            if self.waived(rule.name()) {
                continue;
            }
            let has_window = self.windows.iter().any(|w| w.rule == rule.name());
            let mut per_dialog = DialogRequests::default();
            for (lane, detail, offending, _charged) in rule.check_positioned(events) {
                let finding = (rule.name().to_string(), lane, detail);
                // A window taking every request it covers accepts the finding
                // outright; one taking a request per dialog, its dialog's first.
                let accepted_here = has_window
                    && (self.taken_by(rule.name(), offending, &capture, false)
                        || (self.taken_by(rule.name(), offending, &capture, true)
                            && per_dialog.first_of_its_dialog(offending, &capture)));
                if accepted_here {
                    accepted.push(finding);
                } else {
                    gating.push(finding);
                }
            }
        }
        (gating, accepted)
    }
}

/// The one accepted request of each dialog: its method and CSeq number.
#[derive(Default)]
struct DialogRequests(HashMap<DialogKey, (Method, u32)>);

impl DialogRequests {
    /// Whether the request at `offending` is the first of its dialog accepted,
    /// or a copy of it on another hop; an unreadable entry is not.
    fn first_of_its_dialog(&mut self, offending: Option<usize>, capture: &CaptureIndex) -> bool {
        let message = offending.and_then(|i| i.checked_sub(1)).and_then(|i| capture.message(i));
        let Some((req, method)) = message.and_then(|m| Some((m, m.method()?))) else {
            return false;
        };
        let request = (method.clone(), req.cseq);
        let first = self.0.entry(req.dialog()).or_insert_with(|| request.clone());
        *first == request
    }
}
