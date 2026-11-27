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

/// `(socket, dialog)` → the first BYE, or 2xx to one, on that dialog
/// ([`dialog_of`]) between that socket and its peer: from there on, both ends
/// hold the dialog ended for new requests.
type EndedDialogs<'a> = BTreeMap<(&'a str, (&'a str, [&'a str; 2])), DialogEnd<'a>>;

/// Where a dialog ended in the view, and the sockets of the BYE's UAC and UAS.
#[derive(Clone, Copy)]
struct DialogEnd<'a> {
    msg: usize,
    uac: &'a str,
    uas: &'a str,
}

/// Whether `msg` ends its dialog at both its ends: a BYE the strict reading
/// admits, whose sender and taker hold the dialog ended for new requests once
/// it is sent or taken (RFC 3261 §15.1.1, RFC 5407 §3.3), or a 2xx to a BYE.
fn ends_dialog(msg: &Msg) -> bool {
    match msg.status() {
        Some(status) => msg.cseq_method.eq_ignore_ascii_case("BYE") && (200..300).contains(&status),
        None => msg.is_request("BYE") && !msg.refused,
    }
}

/// A transaction of the §8.2 walk: its [`uas_key`], and whether it is a
/// CANCEL's, which rides its INVITE's branch as a transaction of its own
/// (RFC 3261 §9.2), so a CANCEL's response never answers the INVITE.
type WalkKey<'a> = (TxnKey<'a>, bool);

fn walk_key<'a>(sides: &Sides<'a>, msg: &'a Msg) -> Option<WalkKey<'a>> {
    Some((uas_key(sides, msg)?, msg.cseq_method.eq_ignore_ascii_case("CANCEL")))
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
/// applies those checks only to a request that matched one (§12.2.2). A
/// request that follows a BYE on its dialog in the view reached a UAS that
/// held the dialog ended, and its 481 discharges the obligation. One that
/// precedes the BYE was taken on the live dialog and still owes the
/// rejection (§15.1.2), unless the BYE is its UAS's own: the request may have
/// crossed it, as its sender's vantage shows, and that 481 is `Undecidable`.
pub(super) fn rejections<'a>(
    wire: &WireView<'a>,
    rule: RuleId,
    owes: impl Fn(&Msg) -> bool,
    owed_header: &str,
    judge: impl Fn(&Rejectable<'a>, Option<Answer>) -> (bool, Evidence),
) -> Vec<Finding> {
    let sides = Sides::of(wire.msgs);
    let mut occasions: Vec<Rejectable<'a>> = Vec::new();
    let mut opened: BTreeMap<WalkKey<'a>, usize> = BTreeMap::new();
    let mut answers: BTreeMap<WalkKey<'a>, Answer> = BTreeMap::new();
    let mut ended: EndedDialogs<'a> = BTreeMap::new();
    for (mi, msg) in wire.msgs.iter().enumerate() {
        if let Some(dialog) = dialog_of(msg).filter(|_| ends_dialog(msg)) {
            let end = DialogEnd { msg: mi, uac: sides.uac(msg), uas: sides.uas(msg) };
            for socket in [msg.src.as_str(), msg.dst.as_str(), end.uas, end.uac] {
                ended.entry((socket, dialog)).or_insert(end);
            }
        }
        match &msg.kind {
            Kind::Request { method } => {
                // A request the strict reading refuses owes its 400, not the
                // rejection this rule names.
                if msg.refused || !owes(msg) {
                    continue;
                }
                let Some(key) = walk_key(&sides, msg) else { continue };
                if let Entry::Vacant(e) = opened.entry(key) {
                    e.insert(occasions.len());
                    occasions.push(Rejectable {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        cseq: msg.cseq,
                        method: method.as_str(),
                        branch: key.0 .2,
                        emitter: msg.dst.as_str(),
                        taker: msg.src.as_str(),
                        request: msg,
                    });
                }
            }
            Kind::Response { status } if *status >= 200 => {
                let Some(key) = walk_key(&sides, msg) else { continue };
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
            let answer =
                walk_key(&sides, occasion.request).and_then(|key| answers.get(&key)).copied();
            let end_before_481 = dialog_of(occasion.request)
                .and_then(|d| ended.get(&(occasion.emitter, d)))
                .filter(|end| answer.is_some_and(|a| a.status == 481 && end.msg < a.msg));
            let from_its_sender =
                |end: &DialogEnd| end.uac == occasion.taker || end.uas == occasion.emitter;
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
                _ if end_before_481.is_some_and(|end| end.msg < occasion.msg) => {
                    finding(Decision::Compliant)
                }
                _ if end_before_481.is_some_and(|end| !from_its_sender(end)) => {
                    finding(Decision::Undecidable(
                        "the request precedes its UAS's own BYE at this vantage: \
                         it may have crossed it",
                    ))
                }
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
