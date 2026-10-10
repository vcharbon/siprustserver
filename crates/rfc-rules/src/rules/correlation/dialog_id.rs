//! An in-dialog message against the dialog identity its taker holds: tags, peer URI,
//! Call-ID.

use std::collections::{BTreeSet, HashMap};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use crate::rules::Obligation;

use super::peer_dialogs::{charge, PeerDialogs, RELAYED};

/// **§12.2.1.1 — an in-dialog message names the taker's own dialog.** The tag
/// the taker minted is half the dialog identifier; a request whose To-tag, or a
/// response whose From-tag, is not one of them belongs to a dialog the taker
/// does not have, and a real UA answers 481 or drops it.
///
/// **A request is judged only once the peer's tag has appeared** — below that
/// the exchange names no dialog yet. A response is judged from the first one:
/// the From-tag it echoes is the taker's own, minted on the request it answers,
/// so a mismatch is a peer inventing a dialog identity.
///
/// The occasion is one message the taker took CARRYING the tag half in question;
/// a message stating none makes no claim. Charges the endpoint that sent it.
pub struct MidDialogTags;

impl Obligation for MidDialogTags {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut st = PeerDialogs::default();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if !msg.repeat {
                if let Some(dlg) = st.taker_of(msg) {
                    // A request is judged only once the peer's tag has appeared;
                    // a response's From-tag is the taker's own from the first.
                    let (header, tag) = match &msg.kind {
                        Kind::Request { .. } if !dlg.remote_tagged => ("To", None),
                        Kind::Request { .. } => ("To", msg.to_tag.as_ref()),
                        Kind::Response { .. } => ("From", msg.from_tag.as_ref()),
                    };
                    if let Some(tag) = tag {
                        let finding = |d| charge(RuleId::MidDialogTags, msg, mi, d);
                        if dlg.relays() {
                            out.push(finding(Decision::Undecidable(RELAYED)));
                        } else if dlg.local_tags.contains(tag) {
                            out.push(finding(Decision::Compliant));
                        } else {
                            // Sorted and de-duplicated: the set the taker minted,
                            // read the same way whatever order it minted them in.
                            let local_tags: Vec<String> = dlg
                                .local_tags
                                .iter()
                                .cloned()
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect();
                            out.push(finding(Decision::Violated(Evidence::DialogTagForeign {
                                foreign_tag_msg: mi,
                                foreign_tag_hop: msg.hop,
                                foreign_tag_ts_us: msg.at_us,
                                tag_header: header.to_string(),
                                tag: tag.clone(),
                                local_tags,
                            })));
                        }
                    }
                }
            }
            st.note(msg);
        }
        out
    }
}

/// **§12.2.1.1 — the peer's URI is fixed at dialog creation.** The From URI of
/// every in-dialog request a party sends is the URI its dialog-creating INVITE
/// stated; rewriting it mid-dialog breaks the taker's dialog matching, and a
/// real UAS answers 481.
///
/// The occasion is one in-dialog request the taker took on a dialog it saw
/// CREATED (the peer's tag has appeared and the establishing INVITE's From URI
/// is known) — without both, nothing says what URI was owed. Charges the
/// endpoint that sent the request.
pub struct PeerUriStable;

impl Obligation for PeerUriStable {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut st = PeerDialogs::default();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if !msg.repeat && matches!(msg.kind, Kind::Request { .. }) {
                if let Some(dlg) = st.taker_of(msg) {
                    if let (true, Some(dialog_uri)) = (dlg.remote_tagged, &dlg.peer_uri) {
                        let finding = |d| charge(RuleId::PeerUriStable, msg, mi, d);
                        if dlg.relays() {
                            out.push(finding(Decision::Undecidable(RELAYED)));
                        } else {
                            match msg.head.as_deref().and_then(|h| sniff::name_addr_uri(h, "From"))
                            {
                                None => out.push(finding(Decision::Undecidable(
                                    "no readable From URI at this vantage",
                                ))),
                                Some(sent) if sent == *dialog_uri => {
                                    out.push(finding(Decision::Compliant))
                                }
                                Some(sent) => out.push(finding(Decision::Violated(
                                    Evidence::PeerUriRewritten {
                                        peer_uri_msg: mi,
                                        peer_uri_hop: msg.hop,
                                        peer_uri_ts_us: msg.at_us,
                                        method: msg.cseq_method.clone(),
                                        sent_uri: sent,
                                        dialog_uri: dialog_uri.clone(),
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

/// **§12.1 — a dialog keeps the Call-ID it was created with.** The Call-ID is
/// the immutable half of the dialog identifier; a peer that re-identifies a
/// dialog mid-flight leaves the taker holding a dialog nothing will ever match
/// again.
///
/// **Keyed `(taker, From-tag)`, deliberately NOT partitioned on Call-ID** — a
/// partition would open a second bucket for the changed value and make the
/// change invisible. Only a RECEIVED dialog-creating INVITE (method INVITE, no
/// To-tag yet) seeds a From-tag's dialog: seeding on anything else conflates
/// independent out-of-dialog transactions that merely reuse a tag, which is what
/// OPTIONS health probes against a responder minting a constant To-tag do.
///
/// The occasion is one in-dialog message (both tags present) the taker took on a
/// From-tag whose dialog it saw created. Charges the endpoint that sent it.
pub struct DialogCallIdStable;

impl Obligation for DialogCallIdStable {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // (taker, From-tag) -> the Call-ID its dialog-creating INVITE established.
        let mut dialogs: HashMap<(String, String), String> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(from_tag) = msg.from_tag.clone() else { continue };
            let key = (msg.dst.clone(), from_tag);
            match dialogs.get(&key) {
                Some(known) if msg.to_tag.is_some() => {
                    let decision = if *known == msg.call_id {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::DialogCallIdChanged {
                            call_id_msg: mi,
                            call_id_hop: msg.hop,
                            call_id_ts_us: msg.at_us,
                            call_id: msg.call_id.clone(),
                            dialog_call_id: known.clone(),
                        })
                    };
                    out.push(charge(RuleId::DialogCallIdStable, msg, mi, decision));
                }
                Some(_) => {}
                None if msg.is_request("INVITE") && msg.to_tag.is_none() => {
                    dialogs.insert(key, msg.call_id.clone());
                }
                None => {}
            }
        }
        out
    }
}
