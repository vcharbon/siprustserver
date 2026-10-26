//! The dialog judge of one simulated UA: its [`DialogTable`], fed by every
//! datagram the UA sends and every one it sights, and the failing verdicts it
//! recorded. Shared across clones of the UA, like its transaction layer.

use std::sync::{Arc, Mutex};

use sip_message::{SipMessage, SipRequest, SipResponse};
use tokio::time::Instant;

use super::reliable::ReliableSent;
use super::table::{BackwardsCseq, DialogTable};
use super::verdict::DialogVerdict;
use crate::WaiverScope;

/// The opt-ins a run declared for dialog verdicts: the `dialog-…` waivers,
/// shared by the harness with every UA it binds.
pub type DialogOptIns = Arc<Mutex<Vec<WaiverScope>>>;

/// Who answers for the verdicts of a UA's dialog table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogJudging {
    /// The harness: a scripted receive of a request with a failing verdict
    /// errors, and the run fails at its end on any verdict not opted in.
    Harness,
    /// A driver that is the UA's stack (a scenario interpreter): it reads each
    /// verdict and decides; the harness gates none of them.
    Driver(BackwardsCseq),
    /// Nobody: the agent relays requests it is not a party to (a proxy) and
    /// keeps no dialogs.
    Off,
}

struct Inner {
    judging: DialogJudging,
    table: DialogTable,
    reliable: ReliableSent,
    /// Each verdict with when it was first recorded.
    recorded: Vec<(DialogVerdict, Instant)>,
}

/// The dialog table of one UA and the verdicts it recorded.
pub struct DialogJudge {
    party: String,
    opt_ins: DialogOptIns,
    /// The table's clock origin, for the §13.2.2.4 early-dialog window.
    origin: Instant,
    inner: Mutex<Inner>,
}

impl DialogJudge {
    /// A judge for the UA `party`, gated by the harness under `opt_ins`.
    pub fn new(party: impl Into<String>, opt_ins: DialogOptIns, judging: DialogJudging) -> Self {
        let backwards = match judging {
            DialogJudging::Driver(b) => b,
            DialogJudging::Harness | DialogJudging::Off => BackwardsCseq::Verdict,
        };
        DialogJudge {
            party: party.into(),
            opt_ins,
            origin: Instant::now(),
            inner: Mutex::new(Inner {
                judging,
                table: DialogTable::new(backwards),
                reliable: ReliableSent::default(),
                recorded: Vec::new(),
            }),
        }
    }

    /// The UA this judge belongs to.
    pub fn party(&self) -> &str {
        &self.party
    }

    /// Hand the verdicts to `judging` from now on.
    pub fn set_judging(&self, judging: DialogJudging) {
        let mut inner = self.inner.lock().unwrap();
        if let DialogJudging::Driver(backwards) = judging {
            inner.table.set_backwards(backwards);
        }
        inner.judging = judging;
    }

    fn now_us(&self) -> u64 {
        Instant::now().duration_since(self.origin).as_micros() as u64
    }

    /// Feed a message the UA sent.
    pub fn note_sent(&self, msg: &SipMessage) {
        let now_us = self.now_us();
        let mut inner = self.inner.lock().unwrap();
        if inner.judging == DialogJudging::Off {
            return;
        }
        match msg {
            SipMessage::Request(r) => inner.table.note_sent_request(r),
            SipMessage::Response(r) => {
                inner.reliable.note(r);
                inner.table.note_sent_response(r, now_us);
            }
        }
    }

    /// Whether `prack`'s `RAck` names a reliable provisional this UA sent on
    /// the dialog it rides (RFC 3262 §3).
    pub fn prack_names_a_sent_provisional(&self, prack: &SipRequest) -> bool {
        self.inner.lock().unwrap().reliable.names(prack)
    }

    /// Whether the judge is fed at all.
    pub fn is_on(&self) -> bool {
        self.inner.lock().unwrap().judging != DialogJudging::Off
    }

    /// Feed a message the UA received (sighted once, not a §17.2 duplicate)
    /// and record the verdict on a request, if any.
    pub fn sight(&self, msg: &SipMessage) {
        let now_us = self.now_us();
        let mut inner = self.inner.lock().unwrap();
        if inner.judging == DialogJudging::Off {
            return;
        }
        let verdict = match msg {
            SipMessage::Response(r) => inner.table.note_received_response(r, now_us),
            SipMessage::Request(r) => inner.table.judge_received(r, now_us),
        };
        if let Some(v) = verdict {
            if !inner.recorded.iter().any(|(r, _)| *r == v) {
                inner.recorded.push((v, Instant::now()));
            }
        }
    }

    /// The verdict recorded on `request` when it was sighted, if any.
    pub fn verdict_for(&self, request: &SipRequest) -> Option<DialogVerdict> {
        let inner = self.inner.lock().unwrap();
        let tag = |t: Option<&str>| t.unwrap_or_default().to_string();
        inner
            .recorded
            .iter()
            .map(|(v, _)| v)
            .rev()
            .find(|v| {
                !v.kind.on_response()
                    && v.call_id == request.call_id().as_str()
                    && v.method == request.method().as_str()
                    && v.cseq == request.cseq().seq()
                    && v.from_tag == tag(request.from().tag())
                    && v.to_tag == tag(request.to().tag())
            })
            .cloned()
    }

    /// The verdict on `request` that fails a scripted receive: one recorded
    /// under harness judging and not opted in.
    pub fn failure_for(&self, request: &SipRequest) -> Option<DialogVerdict> {
        if self.inner.lock().unwrap().judging != DialogJudging::Harness {
            return None;
        }
        self.verdict_for(request).filter(|v| !self.opted_in(v))
    }

    /// The verdict on `response` that fails a scripted expect: one recorded
    /// when it was sighted, under harness judging and not opted in.
    pub fn response_failure_for(&self, response: &SipResponse) -> Option<DialogVerdict> {
        let inner = self.inner.lock().unwrap();
        if inner.judging != DialogJudging::Harness {
            return None;
        }
        let tag = |t: Option<&str>| t.unwrap_or_default().to_string();
        let found = inner.recorded.iter().map(|(v, _)| v).rev().find(|v| {
            v.kind.on_response()
                && v.call_id == response.call_id().as_str()
                && v.method == response.cseq().method().as_str()
                && v.cseq == response.cseq().seq()
                && v.from_tag == tag(response.from().tag())
                && v.to_tag == tag(response.to().tag())
        });
        let found = found.cloned();
        drop(inner);
        found.filter(|v| !self.opted_in(v))
    }

    /// Whether an opt-in of the run covers `v` on this UA.
    fn opted_in(&self, v: &DialogVerdict) -> bool {
        self.opt_ins.lock().unwrap().iter().any(|w| covers(w, &self.party, v))
    }

    /// The dialogs the UA holds early or confirmed, one ID each.
    pub fn live_dialogs(&self) -> Vec<String> {
        self.inner.lock().unwrap().table.live()
    }

    /// The UA's table and the verdicts it recorded, for a failure dump: one
    /// line per dialog, then one per verdict. Empty for a UA that keeps no
    /// table.
    pub fn summary(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        if inner.judging == DialogJudging::Off {
            return Vec::new();
        }
        let mut lines: Vec<String> = inner.table.rows();
        if lines.is_empty() {
            lines.push("no dialog".to_string());
        }
        let judged_by = match inner.judging {
            DialogJudging::Harness => "",
            DialogJudging::Driver(_) | DialogJudging::Off => " (its driver answers for it)",
        };
        for (v, _) in &inner.recorded {
            lines.push(format!("verdict [{}]{judged_by}: {v}", v.rule()));
        }
        lines
    }

    /// Every verdict the harness gates — those recorded under harness
    /// judging — with when it was first recorded.
    pub fn gated(&self) -> Vec<(DialogVerdict, Instant)> {
        let inner = self.inner.lock().unwrap();
        match inner.judging {
            DialogJudging::Harness => inner.recorded.clone(),
            DialogJudging::Driver(_) | DialogJudging::Off => Vec::new(),
        }
    }
}

/// Whether the waiver `w` opts the verdict `v` of the UA `party` out: the
/// same rule, naming that party, on the method it names if any. A dialog
/// verdict is a finding against the SUT, so a waiver naming no party covers
/// none.
pub fn covers(w: &WaiverScope, party: &str, v: &DialogVerdict) -> bool {
    w.rule == v.rule()
        && w.position.is_none()
        && w.party.as_deref() == Some(party)
        && w.method.as_deref().is_none_or(|m| m == v.method)
}

/// [`covers`], for the verdict `v` first recorded at `at`: a waiver that
/// holds from an instant covers only what was recorded from then on.
pub fn covers_at(w: &WaiverScope, party: &str, v: &DialogVerdict, at: Instant) -> bool {
    covers(w, party, v) && w.since.is_none_or(|since| at >= since)
}

/// Whether `w` waives a dialog rule without naming the UA it is on: it
/// covers nothing, so it is a dead waiver whatever else it says.
pub fn is_partyless_dialog_waiver(w: &WaiverScope) -> bool {
    w.rule.starts_with("dialog-") && w.party.is_none()
}
