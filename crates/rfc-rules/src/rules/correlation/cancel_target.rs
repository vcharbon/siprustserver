//! A CANCEL against the INVITE transaction it cancels (RFC 3261 §9.1).

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::peer_dialogs::{charge, PeerDialogs, RELAYED};

/// **§9.1 — a CANCEL's Request-URI is its INVITE's.** The CANCEL must target
/// exactly the request it cancels; a different URI reaches a different server
/// transaction, or none.
///
/// The INVITE is identified BY BRANCH (§9.1 has the CANCEL reuse it), never by
/// "the first INVITE seen": a dialog legitimately carries the initial INVITE and
/// re-INVITEs. The occasion is one CANCEL the taker took whose branch matches an
/// INVITE it has open — a branch matching none is [`CancelViaBranch`]'s finding,
/// not this one's. Charges the CANCEL's sender.
pub struct CancelRequestUri;

impl Obligation for CancelRequestUri {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut st = PeerDialogs::default();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if !msg.repeat && msg.is_request("CANCEL") {
                if let Some(dlg) = st.taker_of(msg) {
                    let matched = dlg
                        .open_invites
                        .iter()
                        .rev()
                        .find(|(b, _)| b.is_some() && *b == msg.via_branch);
                    if let Some((_, invite_uri)) = matched {
                        let finding = |d| charge(RuleId::CancelRequestUri, msg, mi, d);
                        let cancel_uri = msg.head.as_deref().and_then(sniff::request_uri);
                        if dlg.relays() {
                            out.push(finding(Decision::Undecidable(RELAYED)));
                        } else {
                            match (cancel_uri, invite_uri) {
                                (None, _) | (_, None) => out.push(finding(Decision::Undecidable(
                                    "no readable Request-URI at this vantage",
                                ))),
                                (Some(c), Some(i)) if c == *i => {
                                    out.push(finding(Decision::Compliant))
                                }
                                (Some(c), Some(i)) => out.push(finding(Decision::Violated(
                                    Evidence::CancelUriDiverged {
                                        cancel_uri_msg: mi,
                                        cancel_uri_hop: msg.hop,
                                        cancel_uri_ts_us: msg.at_us,
                                        cancel_uri: c,
                                        invite_uri: i.clone(),
                                    },
                                ))),
                            }
                        }
                    }
                }
            }
            st.note(msg);
        }
        out
    }
}

/// **§9.1 — a CANCEL's top Via branch is an open INVITE's.** The CANCEL carries
/// the branch of the INVITE server transaction it cancels, which is how it is
/// routed to that transaction; a branch matching none of the taker's open
/// INVITEs orphans it, and the INVITE it meant to cancel rings on.
///
/// A dialog legitimately carries several INVITE transactions, so matching ANY of
/// them discharges the obligation. The occasion is one CANCEL the taker took
/// having seen at least one INVITE and carrying a branch — without either,
/// nothing can say the CANCEL is orphaned. Charges the CANCEL's sender.
pub struct CancelViaBranch;

impl Obligation for CancelViaBranch {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut st = PeerDialogs::default();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if !msg.repeat && msg.is_request("CANCEL") {
                if let (Some(dlg), Some(branch)) = (st.taker_of(msg), msg.via_branch.as_deref()) {
                    if !dlg.open_invites.is_empty() {
                        let finding = |d| charge(RuleId::CancelViaBranch, msg, mi, d);
                        if dlg.relays() {
                            out.push(finding(Decision::Undecidable(RELAYED)));
                        } else if dlg.open_invites.iter().any(|(b, _)| b.as_deref() == Some(branch))
                        {
                            out.push(finding(Decision::Compliant));
                        } else {
                            out.push(finding(Decision::Violated(
                                Evidence::CancelBranchUnmatched {
                                    cancel_branch_msg: mi,
                                    cancel_branch_hop: msg.hop,
                                    cancel_branch_ts_us: msg.at_us,
                                    cancel_branch: branch.to_string(),
                                    invite_branches: dlg
                                        .open_invites
                                        .iter()
                                        .filter_map(|(b, _)| b.clone())
                                        .collect(),
                                },
                            )));
                        }
                    }
                }
            }
            st.note(msg);
        }
        out
    }
}
