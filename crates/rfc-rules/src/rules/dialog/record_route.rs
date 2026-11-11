//! RFC 3261 §12.1.1 / §12.2.2 — where a response may carry Record-Route.

use std::collections::BTreeMap;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use super::super::sides::Sides;
use super::super::Obligation;

/// **§12.1.1 / §12.2.2 — Record-Route rides dialog-CREATING responses only.** A
/// 100 Trying creates no dialog, so a Record-Route on it is vestigial; and once
/// a dialog exists its route set is fixed, so a response to an IN-DIALOG request
/// cannot carry one either — there is no longer a set for it to join.
///
/// The occasion is ONE response the endpoint TOOK. Charges the endpoint that
/// sent it. The request it answers is the one of its transaction (Call-ID,
/// top-Via branch, CSeq method) its UAC sent, the UAC named by the socket that
/// request left from ([`Sides`]). A response whose request this vantage never
/// carried settles nothing (`Undecidable`): whether the transaction was
/// in-dialog is exactly what decides it.
///
/// **An ACK is never a correlation target.** It elicits no response, and a
/// non-2xx ACK reuses its INVITE's branch (§17.1.1.3) while carrying the final's
/// To tag — recording it would let a §17.2.1-retransmitted final that crosses
/// the ACK be misread as answering an "in-dialog ACK".
///
/// This is the teeth for a B2BUA or proxy that leaks Record-Route onto these
/// responses; the test UA ignores it, a strict UAC's route-set bookkeeping does
/// not.
pub struct RecordRoutePlacement;

impl Obligation for RecordRoutePlacement {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Requests each UAC SENT, by transaction, with whether they were
        // in-dialog when they went out — built as the walk goes, so a response
        // is only ever measured against a request that preceded it.
        let sides = Sides::of(wire.msgs);
        let mut sent: BTreeMap<(&str, &str, &str, String), (&str, bool)> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { method } => {
                    if !method.eq_ignore_ascii_case("ACK") {
                        if let Some(branch) = branch {
                            sent.insert(
                                (
                                    sides.uac(msg),
                                    msg.call_id.as_str(),
                                    branch,
                                    msg.cseq_method.to_ascii_uppercase(),
                                ),
                                (
                                    method.as_str(),
                                    msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()),
                                ),
                            );
                        }
                    }
                }
                Kind::Response { status } => {
                    let finding = |decision| Finding {
                        rule: RuleId::RecordRoutePlacement,
                        emitter: msg.src.to_string(),
                        taker: msg.dst.to_string(),
                        cseq: msg.cseq,
                        relayed: false,
                        anchor: mi,
                        decision,
                    };
                    let Some(head) = msg.head.as_deref() else {
                        out.push(finding(Decision::Undecidable("no header block at this vantage")));
                        continue;
                    };
                    let rows = sniff::header_values(head, "Record-Route");
                    let Some(row) = rows.into_iter().next() else {
                        out.push(finding(Decision::Compliant));
                        continue;
                    };
                    let misplaced = |request_method: &str| {
                        Decision::Violated(Evidence::RecordRouteMisplaced {
                            record_route_msg: mi,
                            record_route_hop: msg.hop,
                            record_route_ts_us: msg.at_us,
                            status: *status,
                            record_route: row.clone(),
                            request_method: request_method.to_string(),
                        })
                    };
                    if *status == 100 {
                        out.push(finding(misplaced("")));
                        continue;
                    }
                    let opened = branch.and_then(|b| {
                        let method = msg.cseq_method.to_ascii_uppercase();
                        sent.get(&(sides.uac(msg), msg.call_id.as_str(), b, method))
                    });
                    match opened {
                        None => out.push(finding(Decision::Undecidable(
                            "no request on this transaction at this vantage",
                        ))),
                        Some((method, true)) => out.push(finding(misplaced(method))),
                        Some(_) => out.push(finding(Decision::Compliant)),
                    }
                }
            }
        }
        out
    }
}
