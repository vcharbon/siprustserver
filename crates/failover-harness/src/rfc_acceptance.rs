//! **RFC-audit acceptance scoping** for the failover harness: which recorded
//! deviations gate the run and which are declared-accepted.
//!
//! Two scopes, coarse to fine:
//! - a **lifetime waiver** — the rule is not judged at all on this run (no
//!   baseline left in the recording);
//! - an **acceptance window** — the rule is accepted only on messages captured
//!   inside the window (opened at a strike), and gates everywhere else,
//!   so establishment and every fault-free scenario still judge it in full.
//!
//! A window is anchored on the recording's **capture order** (`seq`), not on a
//! timestamp: under a paused clock many messages share one `at_ms`/`sent_ms`, so
//! only `seq` can say "before the arm" exactly (`layer_harness::EventSequencer`).
//!
//! A finding is placed in a window through its `RfcFinding::offending` wire-entry
//! index; a finding the rule cannot pin to an entry is unattributable and gates.
//! Every window is opened by one row of the window table ([`crate::windows`]),
//! and an accepted finding names it.

use std::collections::{BTreeMap, HashMap, HashSet};

use layer_harness::Stamped;
use sip_message::Method;
use sip_net_testkit::{CaptureIndex, DialogKey, SignalingNetworkEvent};

use crate::windows::{Kind, Row, TABLE};

/// One RFC-audit rule accepted over a bounded slice of the run, in capture
/// order, by one row of the window table.
struct AcceptanceWindow {
    /// The window-table row that opened it.
    row: &'static str,
    /// The row's place in the window table: windows take findings in it.
    order: usize,
    /// The audit rule `name()` accepted inside the window.
    rule: &'static str,
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
    /// The window closes when the test declares its incident over.
    closes_when_over: bool,
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

/// A finding a window accepted, with the window-table row that took it.
pub type Accepted = (Finding, &'static str);

/// `(rule, lane, detail)` findings reduced to the `(lane, detail)` pairs the
/// assertion messages render.
pub fn lane_details(findings: Vec<Finding>) -> Vec<(String, String)> {
    findings.into_iter().map(|(_, lane, detail)| (lane, detail)).collect()
}

/// The declared RFC-audit scoping of one harness run: the lifetime waivers plus
/// the acceptance windows, and the classification they imply.
#[derive(Default)]
pub struct RfcAcceptance {
    /// Rule `name()`s waived for the whole run, each with its justification.
    waived: BTreeMap<String, String>,
    /// Window-scoped acceptances, in the order their rows stand in the window
    /// table; outside every window the rule gates in full.
    windows: Vec<AcceptanceWindow>,
}

impl RfcAcceptance {
    /// Stop judging `rule` for the rest of the run (the coarse scope), for
    /// the reason `justification` states.
    pub fn waive_lifetime(&mut self, rule: &str, justification: &str) {
        self.waived.insert(rule.to_string(), justification.to_string());
    }

    /// The lifetime waivers, each rule with its justification.
    pub fn lifetime_waivers(&self) -> &BTreeMap<String, String> {
        &self.waived
    }

    /// Open the windows of `row`, an RFC-window row, over every message
    /// captured AFTER `from_seq` (the recording's high-water mark at the
    /// strike): one per rule it names.
    pub fn open_row(&mut self, row: &'static Row, from_seq: u64) {
        let Kind::Rfc { rules, once_per_dialog, admits } = row.kind else { return };
        let order = TABLE.iter().position(|r| std::ptr::eq(r, row)).unwrap_or(usize::MAX);
        for rule in rules {
            let at = self.windows.partition_point(|w| w.order <= order);
            self.windows.insert(
                at,
                AcceptanceWindow {
                    row: row.id,
                    order,
                    rule,
                    from_seq,
                    until_seq: None,
                    once_per_dialog,
                    admits,
                    closes_when_over: row.closes_when_over,
                },
            );
        }
    }

    /// Close at `until_seq` every open window whose row closes when its
    /// incident is over.
    pub fn close_over(&mut self, until_seq: u64) {
        for w in self.windows.iter_mut().filter(|w| w.closes_when_over && w.until_seq.is_none()) {
            w.until_seq = Some(until_seq);
        }
    }

    /// Whether `rule` carries a lifetime waiver.
    pub fn waived(&self, rule: &str) -> bool {
        self.waived.contains_key(rule)
    }

    /// The row whose window takes the finding of `rule` whose offending
    /// message is the 1-based `offending` index into `capture`, the first in
    /// table order. A finding with no pinned message is never accepted — it
    /// cannot be placed in the run.
    pub fn accepts(
        &self,
        rule: &str,
        offending: Option<usize>,
        capture: &CaptureIndex,
    ) -> Option<&'static str> {
        self.taker(rule, offending, capture, &mut DialogRequests::default())
    }

    /// The row whose window takes the finding, the first in table order. The
    /// windows of `rule` that take one request per dialog share it: they take
    /// only the first request of a dialog any of them took (`per_dialog`).
    fn taker(
        &self,
        rule: &str,
        offending: Option<usize>,
        capture: &CaptureIndex,
        per_dialog: &mut DialogRequests,
    ) -> Option<&'static str> {
        let i = offending.and_then(|i| i.checked_sub(1))?;
        let mut first = None;
        self.windows.iter().filter(|w| w.rule == rule && w.takes(i, capture)).find_map(|w| {
            let taken = !w.once_per_dialog
                || *first.get_or_insert_with(|| per_dialog.first_of_its_dialog(offending, capture));
            taken.then_some(w.row)
        })
    }

    /// Run the cross-message audit over `events` (already the cross-audit
    /// endpoint-scoped view, the binds of `scoped_out` taken out of it) and
    /// split its non-advisory findings into `(gating, accepted)`: a finding is
    /// accepted, by the row of the first window taking it, when its OFFENDING
    /// message was captured inside a window for its rule; a lifetime-waived
    /// rule is dropped outright; an unattributable finding gates.
    pub fn partition(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
        scoped_out: &HashSet<String>,
    ) -> (Vec<Finding>, Vec<Accepted>) {
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
            if rule.force_advisory() || self.waived(rule.name()) {
                continue;
            }
            let mut per_dialog = DialogRequests::default();
            for (lane, detail, offending, _charged) in rule.check_positioned(events) {
                let finding = (rule.name().to_string(), lane, detail);
                match self.taker(rule.name(), offending, &capture, &mut per_dialog) {
                    Some(row) => accepted.push((finding, row)),
                    None => gating.push(finding),
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

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

    use super::*;
    use crate::windows::row;

    fn entry(seq: u64, raw: String) -> RecordedSipEntry {
        RecordedSipEntry {
            seq,
            from: "127.0.0.1:5080".parse().unwrap(),
            to: "127.0.0.1:5070".parse().unwrap(),
            raw: raw.into_bytes(),
            sent_ms: seq,
            received_ms: None,
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: None,
            to_lane: None,
        }
    }

    fn request(method: &str, cseq: u32, branch: &str) -> String {
        format!(
            "{method} sip:b@x SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
    }

    fn opened(ids: &[&str], from_seq: u64) -> RfcAcceptance {
        let mut acceptance = RfcAcceptance::default();
        for id in ids {
            acceptance.open_row(row(id).expect("a row of the table"), from_seq);
        }
        acceptance
    }

    #[test]
    fn the_first_row_in_table_order_takes_a_finding() {
        // Struck in the other order: the table, not the strikes, orders them.
        let acceptance = opened(&["W-split-cseq", "W-split-two-byes"], 0);
        let capture = CaptureIndex::new(vec![
            entry(1, request("BYE", 4, "z9hG4bK-a")),
            entry(2, request("BYE", 4, "z9hG4bK-b")),
            entry(3, request("INFO", 4, "z9hG4bK-c")),
        ]);
        let rule = crate::RULE_CSEQ_IN_DIALOG_ORDER;
        assert_eq!(acceptance.accepts(rule, Some(2), &capture), Some("W-split-two-byes"));
        assert_eq!(acceptance.accepts(rule, Some(3), &capture), Some("W-split-cseq"));
    }

    #[test]
    fn a_window_takes_only_what_its_row_admits_and_from_the_strike_on() {
        let acceptance = opened(&["W-split-two-byes"], 1);
        let capture = CaptureIndex::new(vec![
            entry(1, request("BYE", 4, "z9hG4bK-a")),
            entry(2, request("INFO", 4, "z9hG4bK-b")),
            entry(3, request("BYE", 4, "z9hG4bK-c")),
        ]);
        let rule = crate::RULE_CSEQ_IN_DIALOG_ORDER;
        assert_eq!(acceptance.accepts(rule, Some(1), &capture), None, "before the strike");
        assert_eq!(acceptance.accepts(rule, Some(2), &capture), None, "not a BYE");
        assert_eq!(acceptance.accepts(rule, Some(3), &capture), Some("W-split-two-byes"));
        assert_eq!(acceptance.accepts("other-rule", Some(3), &capture), None);
    }

    #[test]
    fn a_once_per_dialog_row_takes_the_first_request_of_a_dialog() {
        let acceptance = opened(&["W-kill-cseq"], 0);
        let capture = CaptureIndex::new(vec![
            entry(1, request("INFO", 4, "z9hG4bK-a")),
            entry(2, request("INFO", 4, "z9hG4bK-b")),
            entry(3, request("UPDATE", 5, "z9hG4bK-c")),
        ]);
        let rule = crate::RULE_CSEQ_IN_DIALOG_ORDER;
        let mut per_dialog = DialogRequests::default();
        let mut take = |i| acceptance.taker(rule, Some(i), &capture, &mut per_dialog);
        assert_eq!(take(1), Some("W-kill-cseq"));
        assert_eq!(take(2), Some("W-kill-cseq"), "a copy of the same request");
        assert_eq!(take(3), None, "a second request of the dialog");
    }

    #[test]
    fn two_once_per_dialog_rows_of_one_rule_take_one_request_of_a_dialog() {
        // A stall's split row and its kill's row both take a CANCEL of an
        // answered INVITE once per dialog: together, still once.
        let acceptance = opened(&["W-split-cancel-answered", "W-kill-cancel-answered"], 0);
        let answered = |cseq| {
            format!(
                "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i{cseq}\r\n\
                 From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
                 CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
            )
        };
        let capture = CaptureIndex::new(vec![
            entry(1, answered(2)),
            entry(2, answered(3)),
            entry(3, request("CANCEL", 2, "z9hG4bK-c2")),
            entry(4, request("CANCEL", 3, "z9hG4bK-c3")),
        ]);
        let rule = crate::RULE_NO_CANCEL_AFTER_FINAL;
        let mut per_dialog = DialogRequests::default();
        let mut take = |i| acceptance.taker(rule, Some(i), &capture, &mut per_dialog);
        assert_eq!(take(3), Some("W-split-cancel-answered"));
        assert_eq!(take(4), None, "the dialog's second CANCEL, whichever row would take it");
    }

    #[test]
    fn a_row_that_closes_when_its_incident_is_over_takes_nothing_after() {
        let mut acceptance = opened(&["T-lag-copy-acts", "W-split-two-byes"], 0);
        acceptance.close_over(2);
        let capture = CaptureIndex::new(vec![
            entry(1, request("BYE", 4, "z9hG4bK-a")),
            entry(2, request("CANCEL", 1, "z9hG4bK-b")),
            entry(3, request("CANCEL", 1, "z9hG4bK-c")),
            entry(4, request("BYE", 4, "z9hG4bK-d")),
        ]);
        let rule = crate::RULE_NO_CANCEL_AFTER_FINAL;
        assert_eq!(acceptance.accepts(rule, Some(2), &capture), Some("T-lag-copy-acts"));
        assert_eq!(acceptance.accepts(rule, Some(3), &capture), None, "closed when over");
        let cseq = crate::RULE_CSEQ_IN_DIALOG_ORDER;
        assert_eq!(acceptance.accepts(cseq, Some(4), &capture), Some("W-split-two-byes"));
    }
}
