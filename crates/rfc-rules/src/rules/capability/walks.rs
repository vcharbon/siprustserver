//! The two walks the family shares: a request owed a specific rejection, and
//! a response owed the headers its status names.

use std::collections::{btree_map::Entry, BTreeMap};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::super::sides::Sides;
use super::{uas_key, Answer, TxnKey, ANSWER_WINDOW_US};

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

/// `(socket, dialog)` → the view index of the first BYE, or 2xx to one, on
/// that dialog ([`dialog_of`]) between that socket and its peer: from there
/// on, both ends hold the dialog ended for new requests.
type EndedDialogs<'a> = BTreeMap<(&'a str, (&'a str, [&'a str; 2])), usize>;

/// Whether `msg` ends its dialog at both its ends: a BYE the strict reading
/// admits, whose sender and taker hold the dialog ended for new requests once
/// it is sent or taken (RFC 3261 §15.1.1, RFC 5407 §3.3), or a 2xx to a BYE.
fn ends_dialog(msg: &Msg) -> bool {
    match msg.status() {
        Some(status) => msg.cseq_method.eq_ignore_ascii_case("BYE") && (200..300).contains(&status),
        None => msg.is_request("BYE") && !msg.refused,
    }
}

/// The dialog a message names, `(Call-ID, tags)` with its two tags in a fixed
/// order, so both directions of the dialog name it alike; `None` without
/// both tags.
fn dialog_of(msg: &Msg) -> Option<(&str, [&str; 2])> {
    let from = msg.from_tag.as_deref().filter(|t| !t.is_empty())?;
    let to = msg.to_tag.as_deref().filter(|t| !t.is_empty())?;
    let mut tags = [from, to];
    tags.sort_unstable();
    Some((msg.call_id.as_str(), tags))
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
///
/// A UAS matches a To-tagged request to its dialogs before any §8.2 check, and
/// applies those checks only to a request that matched one (§12.2.2). A UAS
/// that sent or took a BYE on the dialog before it answered refuses the
/// request 481 instead, and that 481 discharges the obligation. The 481 is
/// what is weighed against the BYE: a request crossing the UAS's own BYE shows
/// at its sender's vantage before that BYE does.
pub(super) fn rejections<'a>(
    wire: &WireView<'a>,
    rule: RuleId,
    owes: impl Fn(&Msg) -> bool,
    owed_header: &str,
    judge: impl Fn(&Rejectable<'a>, Option<Answer>) -> (bool, Evidence),
) -> Vec<Finding> {
    let sides = Sides::of(wire.msgs);
    let mut occasions: Vec<Rejectable<'a>> = Vec::new();
    let mut opened: BTreeMap<TxnKey<'a>, usize> = BTreeMap::new();
    let mut answers: BTreeMap<TxnKey<'a>, Answer> = BTreeMap::new();
    let mut ended: EndedDialogs<'a> = BTreeMap::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        if let Some(dialog) = dialog_of(msg).filter(|_| ends_dialog(msg)) {
            for socket in [msg.src.as_str(), msg.dst.as_str(), sides.uas(msg), sides.uac(msg)] {
                ended.entry((socket, dialog)).or_insert(mi);
            }
        }
        match &msg.kind {
            Kind::Request { method } => {
                // A request the strict reading refuses owes its 400, not the
                // rejection this rule names.
                if msg.refused || !owes(msg) {
                    continue;
                }
                let Some(key) = uas_key(&sides, msg) else { continue };
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
                let Some(key) = uas_key(&sides, msg) else { continue };
                if !opened.contains_key(&key) {
                    continue;
                }
                answers.entry(key).or_insert(Answer {
                    msg: mi,
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
            let ended_before = |answer: &Answer| {
                dialog_of(occasion.request)
                    .and_then(|d| ended.get(&(occasion.emitter, d)))
                    .is_some_and(|&at| at < answer.msg)
            };
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
                Some(a) if a.status == 481 && ended_before(&a) => finding(Decision::Compliant),
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
/// same status from the same socket on the same transaction is the same
/// answer, not a new occasion; a branchless response is keyed by its call.
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
        let branch = msg.via_branch.as_deref().unwrap_or_default();
        let key = (msg.src.as_str(), msg.call_id.as_str(), branch);
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
