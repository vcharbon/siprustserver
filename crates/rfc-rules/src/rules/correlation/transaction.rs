//! A response against the transaction it answers: its Via stack, its CSeq, and
//! the To-tag its UAS committed.

use std::collections::HashMap;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use crate::rules::charge::charge;
use crate::rules::Obligation;

use super::peer_dialogs::{PeerDialogs, RELAYED};

/// **§8.1.3 / §17.1.3 — a response reproduces its request's Via stack.** A UAC
/// matches a response to its client transaction by the topmost Via branch and a
/// UAS copies the whole stack back unchanged. A response whose top branch names
/// no request the taker sent on that transaction key cannot be matched at all;
/// one whose stack has gained or lost a row was rewritten in transit and will
/// mis-route at the next hop down.
///
/// **Correlation is BRANCH-FIRST, exactly as §17.1.3 defines the client
/// transaction.** Among the requests the taker sent sharing the response's
/// `(Call-ID, CSeq number, CSeq method)`, the one that MINTED the response's top
/// branch is the transaction it belongs to. `(Call-ID, CSeq)` alone is ambiguous
/// where an endpoint legitimately carries several same-key client transactions
/// at once — crossing BYEs that coincide on a CSeq number (§12.2.1.1 lets each
/// side pick its own initial value), or a reroute forking a second INVITE at the
/// same CSeq — so only a response whose top branch matches NONE of them is
/// genuinely unmatchable.
///
/// The occasion is one response the taker took that answers a request it sent;
/// a response correlating to no such request settles nothing and opens none.
/// Both defects ride ONE finding: they are one act of not reproducing the stack.
/// Charges the endpoint that sent the response.
pub struct ResponseEchoesRequestVia;

/// One request an endpoint sent, with the Via stack it stated.
struct SentVia {
    cseq: u32,
    method: String,
    branch: Option<String>,
    vias: usize,
}

impl Obligation for ResponseEchoesRequestVia {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut sent: HashMap<(String, String), Vec<SentVia>> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.call_id.is_empty() {
                continue;
            }
            if matches!(msg.kind, Kind::Request { .. }) {
                if !msg.repeat {
                    sent.entry((msg.src.clone(), msg.call_id.clone())).or_default().push(SentVia {
                        cseq: msg.cseq,
                        method: msg.cseq_method.to_ascii_uppercase(),
                        branch: msg.via_branch.clone(),
                        vias: msg
                            .head
                            .as_deref()
                            .map_or(0, |h| sniff::header_values(h, "Via").len()),
                    });
                }
                continue;
            }
            if msg.repeat {
                continue;
            }
            let method = msg.cseq_method.to_ascii_uppercase();
            let candidates: Vec<&SentVia> = sent
                .get(&(msg.dst.clone(), msg.call_id.clone()))
                .map(|v| v.iter().filter(|r| r.cseq == msg.cseq && r.method == method).collect())
                .unwrap_or_default();
            let Some(&latest) = candidates.last() else {
                continue; // answers no request this endpoint sent — nothing to echo
            };
            let finding = |d| charge(RuleId::ResponseEchoesRequestVia, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let matched = msg
                .via_branch
                .as_deref()
                .and_then(|b| candidates.iter().rev().find(|r| r.branch.as_deref() == Some(b)));
            // A branch-carrying response matching no same-key request is
            // unmatchable at the taker's transaction layer — but only where the
            // taker minted a branch at all, since a branchless legacy request
            // cannot be branch-compared.
            let branch_diverged =
                matched.is_none() && msg.via_branch.is_some() && latest.branch.is_some();
            let reference = matched.copied().unwrap_or(latest);
            let response_vias = sniff::header_values(head, "Via").len();
            if !branch_diverged && response_vias == reference.vias {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::ResponseViaDiverged {
                via_msg: mi,
                via_hop: msg.hop,
                via_ts_us: msg.at_us,
                response_branch: msg.via_branch.clone().unwrap_or_default(),
                request_branch: reference.branch.clone().unwrap_or_default(),
                response_vias,
                request_vias: reference.vias,
            })));
        }
        out
    }
}

/// **§8.1.3.3 — a response's CSeq echoes a request its taker sent.** A response
/// copies the request's CSeq verbatim, so a `(number, method)` no request the
/// taker ever sent produced is a peer answering a phantom — the taker's
/// transaction layer has nothing to give it to.
///
/// The occasion is one response the taker took on a dialog where it HAD sent at
/// least one request of that method: with none, the mismatch is the method's own
/// concern and not this rule's. Charges the endpoint that sent the response.
pub struct ResponseCorrelation;

impl Obligation for ResponseCorrelation {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut st = PeerDialogs::default();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if let (false, Kind::Response { .. }, Some(dlg)) =
                (msg.repeat, &msg.kind, st.taker_of(msg))
            {
                let method = msg.cseq_method.to_ascii_uppercase();
                let finding = |d| charge(RuleId::ResponseCorrelation, msg, mi, d);
                if dlg.relays() {
                    out.push(finding(Decision::Undecidable(RELAYED)));
                } else if dlg.sent_requests.iter().any(|(m, s)| *m == method && *s == msg.cseq) {
                    out.push(finding(Decision::Compliant));
                } else {
                    let sent_cseqs: Vec<u32> = dlg
                        .sent_requests
                        .iter()
                        .filter(|(m, _)| *m == method)
                        .map(|(_, s)| *s)
                        .collect();
                    if !sent_cseqs.is_empty() {
                        out.push(finding(Decision::Violated(Evidence::ResponseCseqPhantom {
                            phantom_msg: mi,
                            phantom_hop: msg.hop,
                            phantom_ts_us: msg.at_us,
                            response_cseq: msg.cseq,
                            response_method: method,
                            sent_cseqs,
                        })));
                    }
                }
            }
            st.note(msg);
        }
        out
    }
}

/// **§17.2.1 / §12.1.1 — a UAS keeps its To-tag across one transaction.** Once a
/// UAS commits a To-tag on a provisional it carries that tag on the final of the
/// same server transaction: the tag names the early dialog the UAC already
/// created, and a final minting a fresh one orphans it with nothing the UAC can
/// reconcile the two by.
///
/// The occasion is one final response the endpoint sent on a transaction (top-Via
/// branch AND CSeq method — §17.2.3 gives a CANCEL its own server transaction on
/// the INVITE's branch) whose provisionals had already established a tag; a final
/// on a transaction with no prior tagged provisional establishes the dialog itself
/// and owes nothing. Charges the responding UAS.
pub struct TagConsistency;

impl Obligation for TagConsistency {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // (emitter, branch, CSeq method) -> the tags its provisionals established,
        // in order.
        let mut committed: HashMap<(String, String, String), Vec<String>> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Response { status } = msg.kind else { continue };
            let (Some(branch), Some(tag)) = (msg.via_branch.clone(), msg.to_tag.clone()) else {
                continue;
            };
            let key = (msg.src.clone(), branch.clone(), msg.cseq_method.to_ascii_uppercase());
            if (101..200).contains(&status) {
                committed.entry(key).or_default().push(tag);
                continue;
            }
            if status < 200 {
                continue;
            }
            let Some(priors) = committed.get(&key).filter(|p| !p.is_empty()) else {
                continue;
            };
            let decision = if priors.contains(&tag) {
                Decision::Compliant
            } else {
                let mut provisional_tags: Vec<String> = Vec::new();
                for p in priors {
                    if !provisional_tags.contains(p) {
                        provisional_tags.push(p.clone());
                    }
                }
                Decision::Violated(Evidence::UasTagFlipped {
                    tag_flip_msg: mi,
                    tag_flip_hop: msg.hop,
                    tag_flip_ts_us: msg.at_us,
                    status,
                    branch,
                    final_tag: tag,
                    provisional_tags,
                })
            };
            out.push(charge(RuleId::TagConsistency, msg, mi, decision));
        }
        out
    }
}
