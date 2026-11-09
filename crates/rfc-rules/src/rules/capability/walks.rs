//! The two walks the family shares: a request owed a specific rejection, and
//! a response owed the headers its status names.

use std::collections::{btree_map::Entry, BTreeMap};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::{sent_key, taken_key, Answer, TxnKey, ANSWER_WINDOW_US};

/// One request an endpoint took that owes a specific rejection.
pub(super) struct Rejectable<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) cseq: u32,
    pub(super) method: &'a str,
    pub(super) branch: &'a str,
    pub(super) emitter: &'a str,
    pub(super) taker: &'a str,
    pub(super) request: &'a Msg,
}

/// The §8.2 walk: pair every request `owes` selects with the FIRST final its
/// taker answered on that transaction, then let `judge` say whether the answer
/// discharged the obligation and what the evidence is. A request the strict
/// reading refuses ([`Msg::refused`]) is no occasion: the stack answers it 400
/// or drops it, whatever else it states.
///
/// The first FINAL, not the first response: a 100 Trying answers nothing about
/// the rejection, and reading it as the answer would charge a UAS that went on
/// to reject correctly.
pub(super) fn rejections<'a>(
    wire: &WireView<'a>,
    rule: RuleId,
    owes: impl Fn(&Msg) -> bool,
    owed_header: &str,
    judge: impl Fn(&Rejectable<'a>, Option<Answer>) -> (bool, Evidence),
) -> Vec<Finding> {
    let mut occasions: Vec<Rejectable<'a>> = Vec::new();
    let mut opened: BTreeMap<TxnKey<'a>, usize> = BTreeMap::new();
    let mut answers: BTreeMap<TxnKey<'a>, Answer> = BTreeMap::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        match &msg.kind {
            Kind::Request { method } => {
                // A request the strict reading refuses owes its 400, not the
                // rejection this rule names.
                if msg.refused || !owes(msg) {
                    continue;
                }
                let Some(key) = taken_key(msg) else { continue };
                if let Entry::Vacant(e) = opened.entry(key) {
                    e.insert(occasions.len());
                    occasions.push(Rejectable {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        cseq: msg.cseq,
                        method: method.as_str(),
                        branch: key.2,
                        emitter: msg.dst.as_str(),
                        taker: msg.src.as_str(),
                        request: msg,
                    });
                }
            }
            Kind::Response { status } if *status >= 200 => {
                let Some(key) = sent_key(msg) else { continue };
                if !opened.contains_key(&key) {
                    continue;
                }
                answers.entry(key).or_insert(Answer {
                    status: *status,
                    listed_rows: msg
                        .head
                        .as_deref()
                        .map(|head| sniff::header_values(head, owed_header).len() as u32),
                });
            }
            Kind::Response { .. } => {}
        }
    }
    occasions
        .iter()
        .map(|occasion| {
            let key = (occasion.emitter, occasion.request.call_id.as_str(), occasion.branch);
            let answer = answers.get(&key).copied();
            let finding = |decision| Finding {
                rule,
                emitter: occasion.emitter.to_string(),
                taker: occasion.taker.to_string(),
                cseq: occasion.cseq,
                relayed: false,
                anchor: occasion.msg,
                decision,
            };
            match answer {
                Some(Answer { listed_rows: None, .. }) => {
                    finding(Decision::Undecidable("no header block at this vantage"))
                }
                None if !wire.obs.absence_decidable(occasion.ts_us, ANSWER_WINDOW_US) => {
                    finding(Decision::Undecidable(
                        "the observation stopped inside the window — \
                                           truncation, not silence",
                    ))
                }
                answer => match judge(occasion, answer) {
                    (true, _) => finding(Decision::Compliant),
                    (false, evidence) => finding(Decision::Violated(evidence)),
                },
            }
        })
        .collect()
}

/// The single-response walk: every response an endpoint SENT whose status
/// `owes` selects must carry at least one of `owed`. A retransmission of that
/// same status on the same transaction is the same answer, not a new occasion.
pub(super) fn sent_response_owes(
    wire: &WireView<'_>,
    rule: RuleId,
    owes: impl Fn(u16) -> bool,
    owed: &[&str],
) -> Vec<Finding> {
    let mut judged: BTreeMap<(TxnKey<'_>, u16), ()> = BTreeMap::new();
    let mut out = Vec::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        let Kind::Response { status } = &msg.kind else { continue };
        if !owes(*status) {
            continue;
        }
        // A branchless response names no transaction; its own status still
        // keys the repeat collapse at this endpoint's call.
        let key = sent_key(msg).unwrap_or((msg.src.as_str(), msg.call_id.as_str(), ""));
        if judged.insert((key, *status), ()).is_some() {
            continue;
        }
        out.push(response_finding(rule, mi, msg, owed));
    }
    out
}

/// One response's verdict on the headers its status owes: carrying ANY of them
/// meets the obligation, carrying none is the miss, and a lost header block
/// settles nothing.
pub(super) fn response_finding(rule: RuleId, mi: usize, msg: &Msg, owed: &[&str]) -> Finding {
    let finding = |decision| Finding {
        rule,
        emitter: msg.src.to_string(),
        taker: msg.dst.to_string(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    };
    let Some(head) = msg.head.as_deref() else {
        return finding(Decision::Undecidable("no header block at this vantage"));
    };
    let carried = owed.iter().any(|name| match *name {
        // §21.4.15 asks the 421 to LIST what it demands: the header is there
        // but empty states no demand at all.
        "Require" => !sniff::option_tags(head, "Require").is_empty(),
        name => sniff::has_header(head, name),
    });
    if carried {
        return finding(Decision::Compliant);
    }
    finding(Decision::Violated(Evidence::ResponseHeadersMissing {
        response_headers_msg: mi,
        response_headers_hop: msg.hop,
        response_headers_ts_us: msg.at_us,
        status: msg.status().unwrap_or_default(),
        missing: owed.iter().map(|s| s.to_string()).collect(),
        branch: msg.via_branch.clone().unwrap_or_default(),
    }))
}
