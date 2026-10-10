//! The RFC 3262 §3 negotiation: when a UAS may, and must, answer reliably.

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;

/// **RFC 3262 §3 — a UAS handed `Require: 100rel` answers reliably, or says it
/// cannot.** The INVITE demanded the extension, so §3 leaves the UAS one
/// choice of two: every non-100 provisional carries `Require: 100rel` AND an
/// `RSeq`, or the INVITE is rejected `420` naming `100rel` in `Unsupported`.
/// A plain 18x against that demand is neither.
///
/// The occasion is ONE provisional the UAS sent on the demanding transaction.
/// The 420 discharges the WHOLE transaction wherever on it it went out.
/// Charges the UAS.
pub struct RequireReliable1xxOnRequire;

impl Obligation for RequireReliable1xxOnRequire {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, served) in &seen.served {
            if !served.requires_100rel {
                continue;
            }
            let answers = seen.answers_on(key);
            let rejected = answers.iter().any(|a| a.status == 420 && a.rejects_100rel);
            for a in answers.iter().filter(|a| a.answers("INVITE")) {
                if !(101..200).contains(&a.status) {
                    continue;
                }
                out.push(Finding {
                    rule: RuleId::RequireReliable1xxOnRequire,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: if !a.readable {
                        Decision::Undecidable("the vantage recorded no headers for the response")
                    } else if rejected || (a.requires_100rel && a.rseq.is_some()) {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::Unreliable1xx {
                            unreliable_1xx_msg: a.msg,
                            unreliable_1xx_hop: a.hop,
                            unreliable_1xx_ts_us: a.ts_us,
                            status: a.status,
                            invite_msg: served.msg,
                            branch: key.branch.to_string(),
                        })
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — a reliable provisional needs the client's opt-in.** The
/// PRACK machinery is negotiated, not imposed: without `100rel` in the INVITE's
/// `Require` or `Supported`, a UAS answering reliably obliges a UAC that never
/// said it could PRACK, and the provisional retransmits until the transaction
/// dies.
///
/// The occasion is ONE reliable provisional the UAS sent. An INVITE this
/// vantage never carried leaves it undecidable — the negotiation was witnessed
/// by nothing. Charges the UAS.
pub struct ReliableNeedsClientOptIn;

impl Obligation for ReliableNeedsClientOptIn {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list.iter().filter(|a| a.reliable_1xx()) {
                out.push(Finding {
                    rule: RuleId::ReliableNeedsClientOptIn,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: match seen.served.get(key) {
                        None => Decision::Undecidable(
                            "the INVITE this response answers never crossed this vantage",
                        ),
                        Some(invite) if invite.offers_100rel => Decision::Compliant,
                        Some(invite) => Decision::Violated(Evidence::UnsolicitedReliable1xx {
                            unsolicited_1xx_msg: a.msg,
                            unsolicited_1xx_hop: a.hop,
                            unsolicited_1xx_ts_us: a.ts_us,
                            status: a.status,
                            invite_msg: invite.msg,
                            branch: key.branch.to_string(),
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — the PRACK machinery is scoped to the INVITE METHOD, not to
/// the dialog-creating INVITE.** §3 "does not allow reliable provisional
/// responses for any method but INVITE"; §4 bars `Require: 100rel` from every
/// other request and §7 Table 3 permits `RSeq` only in an INVITE response. A
/// re-INVITE is an INVITE: §3's To-tag bar is written for a PROXY and says so
/// ("unlike a UAS"), and RFC 6141 §4.6 has a UAS answer one reliably.
///
/// The occasion is ONE provisional carrying `Require: 100rel` that the UAS sent
/// to a non-INVITE request. A request this vantage never carried leaves it
/// undecidable. Charges the UAS.
pub struct NoReliable1xxOnInDialog;

impl Obligation for NoReliable1xxOnInDialog {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list {
                if !((101..200).contains(&a.status) && a.requires_100rel) {
                    continue;
                }
                out.push(Finding {
                    rule: RuleId::NoReliable1xxOnInDialog,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: match seen.served.get(key) {
                        None => Decision::Undecidable(
                            "the request this response answers never crossed this vantage",
                        ),
                        Some(request) if request.method.eq_ignore_ascii_case("INVITE") => {
                            Decision::Compliant
                        }
                        Some(request) => Decision::Violated(Evidence::InDialogReliable1xx {
                            in_dialog_1xx_msg: a.msg,
                            in_dialog_1xx_hop: a.hop,
                            in_dialog_1xx_ts_us: a.ts_us,
                            status: a.status,
                            method: request.method.to_string(),
                            request_msg: request.msg,
                            branch: key.branch.to_string(),
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
