//! The From-tag of a request an endpoint takes inside a dialog it confirmed.

use std::collections::{BTreeSet, HashMap};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use crate::rules::Obligation;

use super::peer_dialogs::{charge, RELAYED};

/// How long after an endpoint takes the first 2xx to its INVITE an early
/// dialog of another branch still stands: §13.2.2.4 has the UAC terminate the
/// early dialogs no 2xx confirmed 64·T1 (32 s) after that first 2xx.
pub const EARLY_DIALOG_WINDOW_US: u64 = 32_000_000;

/// **§12.2.1.1 / §12.1.2 — a request taken inside a confirmed dialog carries
/// that dialog's remote tag in From.** The 2xx that confirms a dialog fixes
/// its identifier: the UAC reads the remote tag off the 2xx's To, the UAS off
/// the INVITE's From. A request the endpoint then takes under its own tag of
/// that dialog, with a From-tag naming none of the dialogs it confirmed, is
/// in no dialog it holds, and a real UA answers 481.
///
/// The occasion is one request the endpoint TOOK carrying both tags, whose
/// To-tag is the endpoint's own tag of a dialog it has seen confirmed. Not an
/// occasion: a To-tag naming no confirmed dialog of the taker
/// ([`MidDialogTags`](super::MidDialogTags) judges it), and a From-tag naming an early dialog of
/// another branch the taker still holds — a provisional's To-tag, inside
/// [`EARLY_DIALOG_WINDOW_US`] of the first 2xx — on a request the answering
/// side may send there. A BYE (§15) and an INVITE (§14.1: the dialog's INVITE
/// is still in progress) never ride an early dialog from that side, so they
/// stay occasions. A vantage relaying both directions stands `Undecidable`.
/// Charges the endpoint that sent the request.
pub struct InDialogFromTag;

impl Obligation for InDialogFromTag {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut by: HashMap<(String, String), ConfirmedDialogs> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.call_id.is_empty() {
                continue;
            }
            let taker = (msg.dst.clone(), msg.call_id.clone());
            if !msg.repeat {
                if let Some(d) = by.get(&taker).and_then(|c| c.decision(msg, mi)) {
                    out.push(charge(RuleId::InDialogFromTag, msg, mi, d));
                }
            }
            by.entry((msg.src.clone(), msg.call_id.clone())).or_default().observe(msg, true);
            by.entry(taker).or_default().observe(msg, false);
        }
        out
    }
}

/// The dialogs ONE endpoint has seen confirmed on ONE Call-ID, as
/// [`InDialogFromTag`] needs them.
#[derive(Default)]
struct ConfirmedDialogs {
    /// `(local tag, remote tag)` of each confirmed dialog: a 2xx to INVITE
    /// this endpoint took gives `(From-tag, To-tag)`, one it sent
    /// `(To-tag, From-tag)`.
    confirmed: BTreeSet<(String, String)>,
    /// `(local tag, remote tag)` of each early dialog a provisional to its
    /// INVITE gave it.
    early: BTreeSet<(String, String)>,
    /// When it took its first 2xx to INVITE: the early dialogs of the other
    /// branches end [`EARLY_DIALOG_WINDOW_US`] later.
    first_2xx_taken_us: Option<u64>,
    /// The establishing INVITE crossed in this direction.
    sent_establishing: bool,
    took_establishing: bool,
}

impl ConfirmedDialogs {
    /// Forwarding both directions of this Call-ID — see the module doc.
    fn relays(&self) -> bool {
        self.sent_establishing && self.took_establishing
    }

    /// The verdict on a request this endpoint takes, or `None` where no
    /// obligation arises.
    fn decision(&self, msg: &Msg, mi: usize) -> Option<Decision> {
        let Kind::Request { method } = &msg.kind else { return None };
        let (Some(local), Some(remote)) = (msg.to_tag.as_deref(), msg.from_tag.as_deref()) else {
            return None;
        };
        let remotes: Vec<String> =
            self.confirmed.iter().filter(|(l, _)| l == local).map(|(_, r)| r.clone()).collect();
        if remotes.is_empty() {
            return None;
        }
        if self.relays() {
            return Some(Decision::Undecidable(RELAYED));
        }
        if remotes.iter().any(|r| r == remote) {
            return Some(Decision::Compliant);
        }
        let early_standing = self
            .first_2xx_taken_us
            .is_some_and(|t| msg.at_us <= t.saturating_add(EARLY_DIALOG_WINDOW_US));
        let early_request = !["BYE", "INVITE"].iter().any(|m| method.eq_ignore_ascii_case(m));
        if early_standing
            && early_request
            && self.early.contains(&(local.to_string(), remote.to_string()))
        {
            return None;
        }
        Some(Decision::Violated(Evidence::DialogRemoteTagForeign {
            foreign_from_msg: mi,
            foreign_from_hop: msg.hop,
            foreign_from_ts_us: msg.at_us,
            method: method.clone(),
            from_tag: remote.to_string(),
            dialog_remote_tags: remotes,
        }))
    }

    /// Fold one carried message in, from this endpoint's side of it.
    fn observe(&mut self, msg: &Msg, sent: bool) {
        match &msg.kind {
            Kind::Request { method } => {
                if method.eq_ignore_ascii_case("INVITE") && msg.to_tag.is_none() {
                    if sent {
                        self.sent_establishing = true;
                    } else {
                        self.took_establishing = true;
                    }
                }
            }
            Kind::Response { status } => {
                if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                    return;
                }
                let (Some(from), Some(to)) = (msg.from_tag.clone(), msg.to_tag.clone()) else {
                    return;
                };
                let pair = if sent { (to, from) } else { (from, to) };
                match status {
                    101..=199 if !sent => {
                        self.early.insert(pair);
                    }
                    200..=299 => {
                        if !sent && self.first_2xx_taken_us.is_none() {
                            self.first_2xx_taken_us = Some(msg.at_us);
                        }
                        self.confirmed.insert(pair);
                    }
                    _ => {}
                }
            }
        }
    }
}
