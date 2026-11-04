//! The dialog walk the §12 rules share: per endpoint, the requests it sent,
//! each paired with the dialog state that held when it went.

use std::collections::BTreeMap;

use sip_message::sniff::UriFacts;

use crate::verdict::{Decision, Finding, RuleId};
use crate::wire::{Kind, Msg};

use super::message_facts::{record_route_set, remote_tag_of, set_if_empty};

/// One endpoint's view of ONE dialog. Spelled out as a struct so the three
/// parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DialogKey<'a> {
    /// The endpoint whose dialog this is.
    endpoint: &'a str,
    call_id: &'a str,
    /// The PEER's dialog tag, `""` before the peer has minted one.
    remote_tag: &'a str,
}

/// What the establishing exchange fixed for one dialog, as one endpoint saw it.
#[derive(Debug, Default, Clone)]
pub(super) struct Dialog {
    /// The URI this endpoint answers on, and the URI its peer does — §12.2.1.1
    /// has every in-dialog request repeat the pair.
    pub(super) local_uri: String,
    pub(super) remote_uri: String,
    /// The route set in the order in-dialog requests must reproduce it.
    pub(super) route_set: Vec<UriFacts>,
    /// The endpoint sent the establishing INVITE, so its route set is the
    /// dialog-creating RESPONSE's Record-Route stack, without the rows it
    /// forwarded, reversed (§12.1.2).
    pub(super) is_uac: bool,
    /// Record-Route rows the endpoint itself forwarded on the establishing
    /// INVITE: a record-routing proxy's own hop and those upstream of it, the
    /// bottom of the response's stack and no part of its route set.
    pub(super) forwarded_record_routes: usize,
    /// Top-Via branches of the establishing INVITE, in each direction. An
    /// INVITE on the sent branch is that INVITE, not a re-INVITE.
    pub(super) initial_invite_sent_branch: String,
    pub(super) initial_invite_received_branch: String,
    /// The endpoint ANSWERED the establishing INVITE with a tagged response, so
    /// it is this dialog's UAS — the party §15 forbids a BYE to before it has
    /// accepted.
    pub(super) answered_establishing_invite: bool,
    /// One of those answers was a 2xx: the dialog it is the UAS of is
    /// confirmed, not early.
    pub(super) accepted_establishing_invite: bool,
    /// The endpoint, as UAC, took a 2xx on the establishing INVITE's branch:
    /// the dialog is confirmed, and a non-2xx after it on that branch is a
    /// stray that ends nothing (§13.2.2.4, §17.1.1.2).
    pub(super) took_establishing_2xx: bool,
    /// When the endpoint, as UAC, took a non-2xx final on the establishing
    /// INVITE's branch before any 2xx on it (§12.3 ends the early dialog);
    /// cleared by any INVITE 2xx it takes and by a provisional on a later
    /// INVITE (a challenge retried).
    pub(super) rejected_at_us: Option<u64>,
}

/// One request an endpoint SENT, with the dialog state that held when it went.
#[derive(Debug)]
pub(super) struct Sent<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) cseq: u32,
    /// The request-line method, as the wire spelled it.
    pub(super) method: &'a str,
    pub(super) emitter: &'a str,
    pub(super) taker: &'a str,
    pub(super) dialog: Dialog,
    /// This IS the dialog-creating INVITE: the first establishing INVITE this
    /// endpoint's view of the dialog carried, in either direction.
    pub(super) initial_invite: bool,
    /// §12.2.1.1's in-dialog test: both tags present, and — for an INVITE — not
    /// the establishing INVITE by branch.
    pub(super) in_dialog: bool,
    /// The peer's tag is known, so the dialog is confirmed at this endpoint.
    pub(super) confirmed: bool,
}

impl<'a> Sent<'a> {
    pub(super) fn finding(&self, rule: RuleId, decision: Decision) -> Finding {
        Finding {
            rule,
            emitter: self.emitter.to_string(),
            taker: self.taker.to_string(),
            cseq: self.cseq,
            relayed: false,
            anchor: self.msg,
            decision,
        }
    }

    /// ACK and CANCEL take the INVITE's own path (§17.1.1.3 / §9.1), not the
    /// dialog's — the route rules do not judge them.
    pub(super) fn hop_by_hop(&self) -> bool {
        self.method.eq_ignore_ascii_case("ACK") || self.method.eq_ignore_ascii_case("CANCEL")
    }
}

/// What one view's messages say about the dialogs on it: every request each
/// endpoint sent, paired with the dialog state that held when it did.
#[derive(Debug, Default)]
pub(super) struct Reading<'a> {
    pub(super) requests: Vec<Sent<'a>>,
    /// Per dialog, keyed as §12 names it.
    dialogs: BTreeMap<DialogKey<'a>, Dialog>,
}

impl<'a> Reading<'a> {
    pub(super) fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            // A message with no call or no From tag names no dialog: it can key
            // nothing, in either endpoint's view.
            if msg.call_id.is_empty() || msg.from_tag.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            for endpoint in [msg.src.as_str(), msg.dst.as_str()] {
                seen.absorb(mi, msg, endpoint, endpoint == msg.src.as_str());
                if msg.src == msg.dst {
                    break; // one endpoint talking to itself is one view
                }
            }
        }
        seen
    }

    /// Absorb one message into `endpoint`'s view of the dialog it rides:
    /// record the occasion it opens (a request the endpoint sent), then let it
    /// advance the dialog state — the order the check demands, since a request
    /// is judged against what the dialog was BEFORE it.
    fn absorb(&mut self, mi: usize, msg: &'a Msg, endpoint: &'a str, sent: bool) {
        let remote_tag = remote_tag_of(msg, sent);
        let key = DialogKey { endpoint, call_id: msg.call_id.as_str(), remote_tag };
        // A tag first seen now inherits what the pending state fixed: the
        // establishing INVITE the UAC sent before its peer had a tag.
        if !remote_tag.is_empty() && !self.dialogs.contains_key(&key) {
            let pending =
                self.dialogs.get(&DialogKey { remote_tag: "", ..key }).cloned().unwrap_or_default();
            self.dialogs.insert(key, pending);
        }
        let dialog = self.dialogs.entry(key).or_default();

        if let Kind::Request { method } = &msg.kind {
            let branch = msg.via_branch.as_deref().unwrap_or_default();
            let establishing = method.eq_ignore_ascii_case("INVITE")
                && msg.to_tag.as_deref().is_none_or(str::is_empty);
            if sent {
                let initial_invite = method.eq_ignore_ascii_case("INVITE")
                    && dialog.initial_invite_sent_branch.is_empty()
                    && dialog.initial_invite_received_branch.is_empty();
                let both_tags = !remote_tag.is_empty()
                    && msg.from_tag.as_deref().is_some_and(|t| !t.is_empty())
                    && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty());
                let in_dialog = both_tags
                    && !(method.eq_ignore_ascii_case("INVITE")
                        && !branch.is_empty()
                        && branch == dialog.initial_invite_sent_branch);
                self.requests.push(Sent {
                    msg: mi,
                    hop: msg.hop,
                    ts_us: msg.at_us,
                    cseq: msg.cseq,
                    method: method.as_str(),
                    emitter: msg.src.as_str(),
                    taker: msg.dst.as_str(),
                    dialog: dialog.clone(),
                    initial_invite,
                    in_dialog,
                    confirmed: !remote_tag.is_empty(),
                });
            }
            // Only the establishing INVITE fixes role, URIs and route set.
            if !establishing {
                return;
            }
            if sent {
                if dialog.initial_invite_sent_branch.is_empty() {
                    dialog.is_uac = true;
                    dialog.initial_invite_sent_branch = branch.to_string();
                    dialog.forwarded_record_routes = record_route_set(msg).len();
                    set_if_empty(&mut dialog.local_uri, msg, "From");
                    set_if_empty(&mut dialog.remote_uri, msg, "To");
                }
            } else if dialog.initial_invite_received_branch.is_empty() {
                dialog.initial_invite_received_branch = branch.to_string();
                set_if_empty(&mut dialog.local_uri, msg, "To");
                set_if_empty(&mut dialog.remote_uri, msg, "From");
                if dialog.route_set.is_empty() {
                    // §12.1.1: the UAS's route set is the recorded stack in
                    // wire order.
                    dialog.route_set = record_route_set(msg);
                }
            }
            return;
        }

        let Kind::Response { status } = &msg.kind else { return };

        // The UAC's own reading of its establishing INVITE's outcome.
        if !sent && msg.cseq_method.eq_ignore_ascii_case("INVITE") {
            let on_initial = !dialog.initial_invite_sent_branch.is_empty()
                && msg.via_branch.as_deref() == Some(dialog.initial_invite_sent_branch.as_str());
            dialog.took_establishing_2xx |= on_initial && (200..300).contains(status);
            if (200..300).contains(status) || (*status > 100 && *status < 200 && !on_initial) {
                dialog.rejected_at_us = None;
            } else if *status >= 300 && on_initial && !dialog.took_establishing_2xx {
                dialog.rejected_at_us.get_or_insert(msg.at_us);
            }
        }

        // A tagged answer the endpoint SENT on the establishing INVITE's own
        // transaction makes it this dialog's UAS (§15's early-BYE test): the
        // branch is what says the answer is to THAT INVITE and not a later
        // re-INVITE the endpoint happens to be answering.
        if sent
            && msg.cseq_method.eq_ignore_ascii_case("INVITE")
            && !dialog.initial_invite_received_branch.is_empty()
            && msg.via_branch.as_deref() == Some(dialog.initial_invite_received_branch.as_str())
            && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty())
        {
            dialog.answered_establishing_invite = true;
            dialog.accepted_establishing_invite |= (200..300).contains(status);
        }

        // Only the UAC's route set is still open, and only a dialog-CREATING
        // answer to the INVITE closes it (§12.1.2).
        if sent || !dialog.is_uac || !dialog.route_set.is_empty() {
            return;
        }
        let creating = (200..300).contains(status)
            || (*status > 100
                && *status < 200
                && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()));
        if creating && msg.cseq_method.eq_ignore_ascii_case("INVITE") {
            let mut set = record_route_set(msg);
            set.truncate(set.len().saturating_sub(dialog.forwarded_record_routes));
            set.reverse();
            dialog.route_set = set;
        }
    }
}
