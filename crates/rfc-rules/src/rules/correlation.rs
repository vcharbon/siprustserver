//! What a message must name in ONE endpoint's OWN traffic — the obligations
//! that can only be judged from that party's seat, against the dialog and
//! transaction state it built as it sent and received. TEN of them, eight
//! reading a message the endpoint TOOK and two reading one it SENT against the
//! same state:
//!
//!   - [`ResponseEchoesRequestVia`] (§8.1.3 / §17.1.3) — a response reproduces
//!     the Via stack of the request the taker sent, top branch included.
//!   - [`ResponseCorrelation`] (§8.1.3.3) — a response's CSeq is one of a
//!     request the taker actually sent.
//!   - [`MidDialogTags`] (§12.2.1.1) — an in-dialog message names the taker's
//!     dialog with a tag the taker minted.
//!   - [`PeerUriStable`] (§12.2.1.1) — the peer's From URI stays the one its
//!     dialog-creating INVITE stated.
//!   - [`DialogCallIdStable`] (§12.1) — a dialog keeps the Call-ID it was
//!     created with.
//!   - [`CancelRequestUri`] / [`CancelViaBranch`] (§9.1) — a CANCEL names the
//!     INVITE transaction it cancels, by Request-URI and by branch.
//!   - [`TagConsistency`] (§17.2.1 / §12.1.1) — a UAS keeps one To-tag across
//!     the responses of one server transaction.
//!   - [`NoToTagOnInitialRequest`] (§8.1.1.2) — the FIRST request an endpoint
//!     puts on a Call-ID names no peer, so it states no To-tag.
//!   - [`InDialogToTag`] (§12.2.1.1) — once the endpoint has watched a dialog
//!     confirm, the requests it sends on it carry that dialog's remote tag.
//!
//! **The state is per `(endpoint, Call-ID)` and built from that endpoint's own
//! stream** ([`PeerDialogs`]): the tags it minted, the requests it sent, the
//! INVITEs it took, the URI its dialog was created with. Every rule here reads
//! that state as it stood BEFORE the message being judged — evidence the taker
//! itself had at the moment it would have had to reject.
//!
//! **A vantage that relays BOTH directions of one dialog judges none of it.**
//! An endpoint that both sent and took an INVITE on a Call-ID is forwarding, not
//! terminating: the "remote" From and tags on what it receives legitimately
//! alternate between the two parties, so its per-endpoint dialog state conflates
//! them and proves nothing. Those occasions stand `Undecidable`.
//!
//! [`DialogCallIdStable`] deliberately does NOT partition on Call-ID — a
//! partition would make a Call-ID CHANGE invisible by opening a second one —
//! [`TagConsistency`] keys on the server transaction rather than the dialog, and
//! [`InDialogToTag`] needs a witness [`PeerDialogs`] does not carry (which
//! branch established the dialog, which drew a non-2xx), so each walks the view
//! under its own key.

use std::collections::{BTreeSet, HashMap};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

// ---------------------------------------------------------------------------
// The per-endpoint dialog state the family reads
// ---------------------------------------------------------------------------

/// What ONE endpoint knows about ONE Call-ID from its own stream.
#[derive(Default)]
struct PeerDialog {
    /// Tags this endpoint minted: the From-tag of requests it sent, the To-tag
    /// of responses it sent. A received in-dialog message names the dialog with
    /// one of these.
    local_tags: Vec<String>,
    /// `(CSeq method, CSeq number)` of every request it sent.
    sent_requests: Vec<(String, u32)>,
    /// Whether it has seen the peer's tag: below that, nothing names a dialog.
    remote_tagged: bool,
    /// `(top-Via branch, Request-URI)` of every INVITE it took. §9.1 scopes a
    /// CANCEL to ONE INVITE transaction, and a dialog carries several (the
    /// initial INVITE plus re-INVITEs), so the CANCEL rules match by branch
    /// rather than pinning the first INVITE seen.
    open_invites: Vec<(Option<String>, Option<String>)>,
    /// The From URI of the first INVITE it took — the URI its peer is bound to.
    peer_uri: Option<String>,
    /// It both sent and took an INVITE here: a relay, not a dialog endpoint.
    sent_invite: bool,
    took_invite: bool,
}

impl PeerDialog {
    /// Forwarding both directions of this dialog — see the module doc.
    fn relays(&self) -> bool {
        self.sent_invite && self.took_invite
    }

    /// Fold in a message this endpoint SENT.
    fn sent(&mut self, msg: &Msg) {
        match &msg.kind {
            Kind::Request { method } => {
                self.sent_requests.push((msg.cseq_method.to_ascii_uppercase(), msg.cseq));
                if method.eq_ignore_ascii_case("INVITE") {
                    self.sent_invite = true;
                }
                if let Some(t) = &msg.from_tag {
                    self.local_tags.push(t.clone());
                }
            }
            Kind::Response { .. } => {
                if let Some(t) = &msg.to_tag {
                    self.local_tags.push(t.clone());
                }
            }
        }
    }

    /// Fold in a message this endpoint TOOK.
    fn took(&mut self, msg: &Msg) {
        if msg.is_request("INVITE") {
            self.took_invite = true;
            let head = msg.head.as_deref();
            self.open_invites
                .push((msg.via_branch.clone(), head.and_then(sniff::request_uri)));
            if self.peer_uri.is_none() {
                self.peer_uri = head.and_then(|h| sniff::name_addr_uri(h, "From"));
            }
        }
        let tag = match &msg.kind {
            Kind::Response { .. } => &msg.to_tag,
            Kind::Request { .. } => &msg.from_tag,
        };
        if tag.is_some() {
            self.remote_tagged = true;
        }
    }
}

/// The dialog state EVERY endpoint in a view builds, keyed
/// `(endpoint, Call-ID)`. A rule walks the view and reads the taker's entry as
/// it stood before the message it is judging.
#[derive(Default)]
struct PeerDialogs {
    by: HashMap<(String, String), PeerDialog>,
}

impl PeerDialogs {
    /// The taker's state for this message's dialog, as it stands now.
    fn taker_of(&self, msg: &Msg) -> Option<&PeerDialog> {
        self.by.get(&(msg.dst.clone(), msg.call_id.clone()))
    }

    /// Fold `msg` in at BOTH ends: its sender minted what it states, its taker
    /// learned what it carried.
    fn note(&mut self, msg: &Msg) {
        if msg.call_id.is_empty() {
            return;
        }
        self.by
            .entry((msg.src.clone(), msg.call_id.clone()))
            .or_default()
            .sent(msg);
        self.by
            .entry((msg.dst.clone(), msg.call_id.clone()))
            .or_default()
            .took(msg);
    }
}

/// A finding on the message at `mi`, charging the endpoint that emitted it and
/// read at the endpoint that took it.
fn charge(rule: RuleId, msg: &Msg, mi: usize, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: msg.src.clone(),
        taker: msg.dst.clone(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}

/// The `Undecidable` reason a relaying vantage states.
const RELAYED: &str = "this vantage relays both directions of the dialog";

// ---------------------------------------------------------------------------
// The obligations
// ---------------------------------------------------------------------------

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
    fn id(&self) -> RuleId {
        RuleId::ResponseEchoesRequestVia
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut sent: HashMap<(String, String), Vec<SentVia>> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.call_id.is_empty() {
                continue;
            }
            if matches!(msg.kind, Kind::Request { .. }) {
                if !msg.repeat {
                    sent.entry((msg.src.clone(), msg.call_id.clone())).or_default().push(
                        SentVia {
                            cseq: msg.cseq,
                            method: msg.cseq_method.to_ascii_uppercase(),
                            branch: msg.via_branch.clone(),
                            vias: msg
                                .head
                                .as_deref()
                                .map_or(0, |h| sniff::header_values(h, "Via").len()),
                        },
                    );
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
            let branch_diverged = matched.is_none()
                && msg.via_branch.is_some()
                && latest.branch.is_some();
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
    fn id(&self) -> RuleId {
        RuleId::ResponseCorrelation
    }

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
    fn id(&self) -> RuleId {
        RuleId::MidDialogTags
    }

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
    fn id(&self) -> RuleId {
        RuleId::PeerUriStable
    }

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
    fn id(&self) -> RuleId {
        RuleId::DialogCallIdStable
    }

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
    fn id(&self) -> RuleId {
        RuleId::CancelRequestUri
    }

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
    fn id(&self) -> RuleId {
        RuleId::CancelViaBranch
    }

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
                        } else if dlg
                            .open_invites
                            .iter()
                            .any(|(b, _)| b.as_deref() == Some(branch))
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
    fn id(&self) -> RuleId {
        RuleId::TagConsistency
    }

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

/// Methods that can legitimately OPEN a transaction outside a dialog. Every
/// other verb (BYE, ACK, UPDATE, INFO, PRACK, CANCEL) is intrinsically
/// in-dialog, where a To-tag is what the request owes rather than what it may
/// not state.
const DIALOG_INITIATING_METHODS: &[&str] =
    &["INVITE", "REGISTER", "SUBSCRIBE", "OPTIONS", "REFER", "MESSAGE", "PUBLISH", "NOTIFY"];

/// **§8.1.1.2 — a request outside any dialog states no To-tag.** The To-tag is
/// the peer's half of the §12 dialog identifier, and a dialog-initiating request
/// has no peer yet: a tag on one names a dialog that does not exist, and a
/// strict UAS answers 481 rather than creating it.
///
/// **The occasion is the FIRST message an endpoint puts on a Call-ID, in either
/// direction** — that is the point at which nothing yet names a dialog from
/// that endpoint's seat. Once ANY traffic for the Call-ID has crossed, later
/// same-Call-ID requests are in-dialog and a To-tag is legitimate; CANCEL and
/// the other intrinsically in-dialog verbs are no occasion at any point,
/// CANCEL's tag semantics belonging to the §9.1 rules that pair it with its
/// INVITE.
///
/// **"First sighting" is what a vantage can see of "outside a dialog", and the
/// two part company on a vantage that JOINS one.** An endpoint that adopts a
/// dialog it never watched open — an HA worker reclaiming a call across a
/// reboot, a capture started mid-call — puts an in-dialog request on a Call-ID
/// it has no history of, and nothing in the stream tells that apart from a
/// genuine out-of-dialog request. Consumer policy scopes such a lane out; the
/// rule states what its vantage proves.
///
/// Every fact is a wire-model one, so the occasion is always DECIDED. Charges
/// the endpoint that sent the request.
pub struct NoToTagOnInitialRequest;

impl Obligation for NoToTagOnInitialRequest {
    fn id(&self) -> RuleId {
        RuleId::NoToTagOnInitialRequest
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // The (endpoint, Call-ID) pairs the view has already carried traffic for.
        let mut carried: HashMap<(String, String), ()> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.call_id.is_empty() {
                continue;
            }
            let first_for_sender =
                carried.insert((msg.src.clone(), msg.call_id.clone()), ()).is_none();
            carried.insert((msg.dst.clone(), msg.call_id.clone()), ());
            if msg.repeat || !first_for_sender {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            if !DIALOG_INITIATING_METHODS.iter().any(|m| method.eq_ignore_ascii_case(m)) {
                continue;
            }
            let decision = match msg.to_tag.as_deref().filter(|t| !t.is_empty()) {
                None => Decision::Compliant,
                Some(tag) => Decision::Violated(Evidence::ForbiddenHeaderPresent {
                    forbidden_msg: mi,
                    forbidden_hop: msg.hop,
                    forbidden_ts_us: msg.at_us,
                    on: method.clone(),
                    header: "To;tag".to_string(),
                    value: tag.to_string(),
                }),
            };
            out.push(charge(RuleId::NoToTagOnInitialRequest, msg, mi, decision));
        }
        out
    }
}

/// What ONE endpoint watched happen to ONE Call-ID, as [`InDialogToTag`] needs
/// it: enough to say the dialog is confirmed, and which branches are exempt.
#[derive(Default)]
struct DialogWitness {
    /// A To-tagged 2xx to INVITE crossed in either direction — the INVITE is
    /// answered, so a dialog exists.
    answered: bool,
    /// The peer's dialog tag as this endpoint RECEIVED it: the From-tag of a
    /// request sent to it, the To-tag of a response to a request it sent. Empty
    /// until the peer names itself — a 2xx this endpoint sends carries its own
    /// local tag and says nothing about the remote one.
    remote_tag: String,
    /// Top-Via branches of To-tag-less INVITEs: the dialog-ESTABLISHING
    /// transaction, whose retransmissions stay legitimately tag-less even after
    /// the 2xx lands.
    establishing_branches: BTreeSet<String>,
    /// Top-Via branches on which a non-2xx final crossed. The ACK for such a
    /// final is generated by the client transaction on the INVITE's own branch
    /// and copies that response's To verbatim (§17.1.1.3).
    non_2xx_branches: BTreeSet<String>,
    /// The establishing INVITE crossed in this direction.
    sent_establishing: bool,
    took_establishing: bool,
}

impl DialogWitness {
    /// This endpoint carried the establishing INVITE BOTH ways — a transparent
    /// relay for this Call-ID, forwarding a peer's headers unchanged. A missing
    /// tag is then the originating UA's finding, not the relay's.
    fn relays(&self) -> bool {
        self.sent_establishing && self.took_establishing
    }

    /// The dialog is confirmed AND its remote tag exists: the answered INVITE
    /// gives the dialog, the tag the peer named gives the value every later
    /// request must reproduce. A peer that minted no tag leaves the remote tag
    /// null, and §12.2.1.1 then requires the To tag parameter to be OMITTED —
    /// nothing to demand.
    fn confirmed(&self) -> bool {
        self.answered && !self.remote_tag.is_empty()
    }

    /// The verdict on a request this endpoint sends, or `None` where no
    /// obligation arises at all.
    fn decision(&self, msg: &Msg, mi: usize) -> Option<Decision> {
        let Kind::Request { method } = &msg.kind else { return None };
        if !self.confirmed() || self.relays() || method.eq_ignore_ascii_case("CANCEL") {
            return None;
        }
        let branch = msg.via_branch.as_deref();
        if branch.is_some_and(|b| self.establishing_branches.contains(b)) {
            return None;
        }
        if method.eq_ignore_ascii_case("ACK")
            && branch.is_some_and(|b| self.non_2xx_branches.contains(b))
        {
            return None;
        }
        Some(match msg.to_tag.as_deref().filter(|t| !t.is_empty()) {
            Some(_) => Decision::Compliant,
            None => Decision::Violated(Evidence::RequiredHeaderAbsent {
                absent_msg: mi,
                absent_hop: msg.hop,
                absent_ts_us: msg.at_us,
                on: method.clone(),
                header: "To;tag".to_string(),
            }),
        })
    }

    /// Fold one carried message in, from this endpoint's side of it.
    fn observe(&mut self, msg: &Msg, sent: bool) {
        if !sent {
            let peer_tag = match &msg.kind {
                Kind::Request { .. } => &msg.from_tag,
                Kind::Response { .. } => &msg.to_tag,
            };
            if let Some(tag) = peer_tag.as_deref().filter(|t| !t.is_empty()) {
                self.remote_tag = tag.to_string();
            }
        }
        match &msg.kind {
            Kind::Request { method } => {
                if !method.eq_ignore_ascii_case("INVITE")
                    || msg.to_tag.as_deref().is_some_and(|t| !t.is_empty())
                {
                    return;
                }
                if let Some(b) = &msg.via_branch {
                    self.establishing_branches.insert(b.clone());
                }
                if sent {
                    self.sent_establishing = true;
                } else {
                    self.took_establishing = true;
                }
            }
            Kind::Response { status } => {
                if msg.cseq_method.eq_ignore_ascii_case("INVITE")
                    && (200..300).contains(status)
                    && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty())
                {
                    self.answered = true;
                }
                if (300..700).contains(status) {
                    if let Some(b) = &msg.via_branch {
                        self.non_2xx_branches.insert(b.clone());
                    }
                }
            }
        }
    }
}

/// **§12.2.1.1 — a request sent within a dialog carries the dialog's remote
/// tag.** The To-tag is half the dialog identifier: a UAS handed a tag-less
/// in-dialog request cannot match it to the dialog and answers 481. The sender
/// mints the header out of its own dialog state, so its SENT requests are what
/// this judges.
///
/// **The occasion needs the dialog CONFIRMED from the sender's own seat**: a
/// To-tagged 2xx to INVITE answered it, and the peer named the remote tag on a
/// message the sender received. The received tag is what makes the demand
/// well-founded on either side — a UAS reads its own local tag off the 2xx it
/// sends, and a caller that minted no tag leaves the remote tag null, which
/// §12.2.1.1 has the sender express by OMITTING the To tag parameter.
///
/// The exemptions keep it to genuine sender defects, and each is no occasion
/// rather than a pass: a request whose To is a verbatim echo of another message
/// is not the sender's to fill — CANCEL copies the INVITE it cancels (§9.1) and
/// the ACK of a non-2xx final copies that response (§17.1.1.3, correlated by
/// the INVITE branch they share); a retransmission of the dialog-establishing
/// INVITE (same branch) is still the tag-less initial request
/// [`NoToTagOnInitialRequest`] governs; and a relay forwarding a peer's headers
/// unchanged states nothing, so the finding lands on the UA that wrote the
/// header. Requests inside an EARLY dialog are out of scope.
///
/// Every fact is a wire-model one, so the occasion is always DECIDED. Charges
/// the endpoint that sent the request.
pub struct InDialogToTag;

impl Obligation for InDialogToTag {
    fn id(&self) -> RuleId {
        RuleId::InDialogToTag
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut by: HashMap<(String, String), DialogWitness> = HashMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.call_id.is_empty() {
                continue;
            }
            let sender = (msg.src.clone(), msg.call_id.clone());
            if !msg.repeat {
                if let Some(d) = by.get(&sender).and_then(|w| w.decision(msg, mi)) {
                    out.push(charge(RuleId::InDialogToTag, msg, mi, d));
                }
            }
            by.entry(sender).or_default().observe(msg, true);
            by.entry((msg.dst.clone(), msg.call_id.clone())).or_default().observe(msg, false);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! The family's OWN semantics: which message is an occasion, what the
    //! taker's own stream settles, and what a relaying vantage may not judge.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::*;

    const A: &str = "10.0.0.1:5060";
    const B: &str = "10.0.0.2:5070";

    #[allow(clippy::too_many_arguments)]
    fn msg(
        at_us: u64,
        src: &str,
        dst: &str,
        kind: Kind,
        call_id: &str,
        cseq: u32,
        cseq_method: &str,
        branch: Option<&str>,
        from_tag: Option<&str>,
        to_tag: Option<&str>,
        head: String,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind,
            call_id: call_id.to_string(),
            cseq,
            cseq_method: cseq_method.to_string(),
            via_branch: branch.map(str::to_string),
            from_tag: from_tag.map(str::to_string),
            to_tag: to_tag.map(str::to_string),
            head: Some(head.into_bytes()),
            body: Some(Vec::new()),
        }
    }

    /// A request `src` sent, with `vias` Via rows.
    #[allow(clippy::too_many_arguments)]
    fn req(
        at_us: u64,
        src: &str,
        dst: &str,
        method: &str,
        uri: &str,
        cseq: u32,
        branch: &str,
        from_uri: &str,
        from_tag: &str,
        to_tag: Option<&str>,
        vias: usize,
    ) -> Msg {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@h>;tag={t}"),
            None => "<sip:bob@h>".to_string(),
        };
        let mut head = format!("{method} {uri} SIP/2.0\r\n");
        for i in 0..vias.max(1) {
            let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
            head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
        }
        head.push_str(&format!(
            "From: <{from_uri}>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
        ));
        msg(
            at_us,
            src,
            dst,
            Kind::Request { method: method.to_string() },
            "c1",
            cseq,
            method,
            Some(branch),
            Some(from_tag),
            to_tag,
            head,
        )
    }

    /// A response `src` sent, with `vias` Via rows.
    #[allow(clippy::too_many_arguments)]
    fn resp(
        at_us: u64,
        src: &str,
        dst: &str,
        status: u16,
        cseq: u32,
        method: &str,
        branch: &str,
        from_tag: &str,
        to_tag: Option<&str>,
        vias: usize,
    ) -> Msg {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@h>;tag={t}"),
            None => "<sip:bob@h>".to_string(),
        };
        let mut head = format!("SIP/2.0 {status} X\r\n");
        for i in 0..vias {
            let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
            head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
        }
        head.push_str(&format!(
            "From: <sip:alice@h>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
        ));
        msg(
            at_us,
            src,
            dst,
            Kind::Response { status },
            "c1",
            cseq,
            method,
            Some(branch),
            Some(from_tag),
            to_tag,
            head,
        )
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// Only the findings charged to `emitter` — the live consumer's vantage
    /// filter, applied here so a test states one party's verdict.
    fn charged(f: Vec<Finding>, emitter: &str) -> Vec<Finding> {
        f.into_iter().filter(|x| x.emitter == emitter).collect()
    }

    /// A dialog A opens to B: INVITE, 180, 200, and the tags they mint.
    fn opened() -> Vec<Msg> {
        vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        ]
    }

    // ── response-echoes-request-via ─────────────────────────────────────────

    #[test]
    fn a_response_echoing_the_stack_is_compliant() {
        let f = charged(run(&ResponseEchoesRequestVia, &opened()), B);
        assert_eq!(f.len(), 2, "one occasion per response A took: {f:?}");
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    #[test]
    fn a_response_on_a_foreign_branch_is_violated() {
        let mut msgs = opened();
        msgs.truncate(1);
        msgs.push(resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-other", "at", Some("bt"), 1));
        let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
        let Decision::Violated(Evidence::ResponseViaDiverged {
            response_branch,
            request_branch,
            ..
        }) = &f[0].decision
        else {
            panic!("via evidence: {:?}", f[0].decision)
        };
        assert_eq!((response_branch.as_str(), request_branch.as_str()), ("z9hG4bK-other", "z9hG4bK-i"));
    }

    /// A grown Via stack was rewritten in transit, and the count is judged
    /// against the request the BRANCH matched, not the most recent one.
    #[test]
    fn a_grown_via_stack_is_violated() {
        let mut msgs = opened();
        msgs.truncate(1);
        msgs.push(resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 3));
        let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
        let Decision::Violated(Evidence::ResponseViaDiverged {
            response_vias, request_vias, ..
        }) = &f[0].decision
        else {
            panic!("via evidence: {:?}", f[0].decision)
        };
        assert_eq!((*response_vias, *request_vias), (3, 1));
    }

    /// Correlation is branch-first: two same-key client transactions in flight
    /// at once (crossing BYEs on one CSeq) are each answered on their own
    /// branch, and picking "the most recent" would flag a correct wire.
    #[test]
    fn same_key_transactions_correlate_by_branch_not_recency() {
        let msgs = vec![
            req(1_000, A, B, "BYE", "sip:bob@h", 5, "z9hG4bK-x", "sip:alice@h", "at", Some("bt"), 1),
            req(1_500, A, B, "BYE", "sip:bob@h", 5, "z9hG4bK-y", "sip:alice@h", "at", Some("bt"), 1),
            resp(2_000, B, A, 200, 5, "BYE", "z9hG4bK-x", "at", Some("bt"), 1),
        ];
        let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A response answering nothing the endpoint sent has no stack to echo.
    #[test]
    fn an_uncorrelated_response_is_no_occasion() {
        let msgs = vec![resp(1_000, B, A, 200, 9, "INVITE", "z9hG4bK-z", "at", Some("bt"), 1)];
        assert!(run(&ResponseEchoesRequestVia, &msgs).is_empty());
    }

    // ── response-correlation ────────────────────────────────────────────────

    #[test]
    fn a_response_echoing_a_sent_cseq_is_compliant() {
        let f = charged(run(&ResponseCorrelation, &opened()), B);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    #[test]
    fn a_phantom_response_cseq_is_violated() {
        let mut msgs = opened();
        msgs.truncate(1);
        msgs.push(resp(2_000, B, A, 200, 9, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1));
        let f = charged(run(&ResponseCorrelation, &msgs), B);
        let Decision::Violated(Evidence::ResponseCseqPhantom {
            response_cseq, sent_cseqs, ..
        }) = &f[0].decision
        else {
            panic!("correlation evidence: {:?}", f[0].decision)
        };
        assert_eq!((*response_cseq, sent_cseqs.as_slice()), (9, [1].as_slice()));
    }

    /// With no request of that method sent, the mismatch is the method's own
    /// concern and this rule states nothing.
    #[test]
    fn a_response_to_an_unsent_method_is_no_occasion() {
        let msgs = vec![resp(1_000, B, A, 200, 1, "OPTIONS", "z9hG4bK-o", "at", Some("bt"), 1)];
        assert!(run(&ResponseCorrelation, &msgs).is_empty());
    }

    // ── mid-dialog-tags ─────────────────────────────────────────────────────

    #[test]
    fn an_in_dialog_request_naming_a_local_tag_is_compliant() {
        let mut msgs = opened();
        msgs.push(req(4_000, B, A, "BYE", "sip:alice@h", 1, "z9hG4bK-b", "sip:bob@h", "bt", Some("at"), 1));
        let f = charged(run(&MidDialogTags, &msgs), B);
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
        assert!(f.iter().any(|x| x.anchor == 3), "the BYE is an occasion: {f:?}");
    }

    #[test]
    fn an_in_dialog_request_naming_a_foreign_tag_is_violated() {
        let mut msgs = opened();
        msgs.push(req(4_000, B, A, "BYE", "sip:alice@h", 1, "z9hG4bK-b", "sip:bob@h", "bt", Some("nope"), 1));
        let f = charged(run(&MidDialogTags, &msgs), B);
        let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
        let Decision::Violated(Evidence::DialogTagForeign { tag_header, tag, local_tags, .. }) =
            &violated.decision
        else {
            panic!("tags evidence: {:?}", violated.decision)
        };
        assert_eq!((tag_header.as_str(), tag.as_str()), ("To", "nope"));
        assert_eq!(local_tags.as_slice(), ["at"]);
    }

    /// A vantage forwarding BOTH directions of one dialog conflates its two
    /// peers, so its tag state proves nothing.
    #[test]
    fn a_relaying_vantage_decides_nothing() {
        // B takes A's INVITE and forwards one of its own on the same Call-ID.
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(1_500, B, "10.0.0.3:5080", "INVITE", "sip:bob@h", 1, "z9hG4bK-j", "sip:alice@h", "at", None, 1),
            resp(2_000, "10.0.0.3:5080", B, 200, 1, "INVITE", "z9hG4bK-j", "at", Some("ct"), 1),
            req(3_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("nope"), 1),
        ];
        let f: Vec<Finding> =
            run(&MidDialogTags, &msgs).into_iter().filter(|x| x.taker == B).collect();
        assert!(!f.is_empty(), "the occasions still stand: {f:?}");
        assert!(f.iter().all(|x| !x.violated()), "a relay charges nothing: {f:?}");
    }

    // ── peer-uri-stable ─────────────────────────────────────────────────────

    #[test]
    fn a_stable_in_dialog_from_uri_is_compliant() {
        let mut msgs = opened();
        msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1));
        let f = charged(run(&PeerUriStable, &msgs), A);
        assert_eq!(f.len(), 1, "the BYE alone: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_rewritten_in_dialog_from_uri_is_violated() {
        let mut msgs = opened();
        msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:eve@h", "at", Some("bt"), 1));
        let f = charged(run(&PeerUriStable, &msgs), A);
        let Decision::Violated(Evidence::PeerUriRewritten { sent_uri, dialog_uri, .. }) =
            &f[0].decision
        else {
            panic!("uri evidence: {:?}", f[0].decision)
        };
        assert_eq!((sent_uri.as_str(), dialog_uri.as_str()), ("sip:eve@h", "sip:alice@h"));
    }

    /// The establishing INVITE fixes the URI; it is not judged against itself.
    #[test]
    fn the_establishing_invite_is_no_occasion() {
        assert!(run(&PeerUriStable, &opened()).is_empty());
    }

    // ── dialog-call-id-stable ───────────────────────────────────────────────

    #[test]
    fn an_in_dialog_message_keeping_its_call_id_is_compliant() {
        let mut msgs = opened();
        msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1));
        let f = charged(run(&DialogCallIdStable, &msgs), A);
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    #[test]
    fn a_changed_call_id_within_a_dialog_is_violated() {
        let mut msgs = opened();
        let mut bye =
            req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1);
        bye.call_id = "c2".to_string();
        msgs.push(bye);
        let f = charged(run(&DialogCallIdStable, &msgs), A);
        let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
        let Decision::Violated(Evidence::DialogCallIdChanged { call_id, dialog_call_id, .. }) =
            &violated.decision
        else {
            panic!("call-id evidence: {:?}", violated.decision)
        };
        assert_eq!((call_id.as_str(), dialog_call_id.as_str()), ("c2", "c1"));
    }

    /// Out-of-dialog transactions reusing a From-tag (OPTIONS probes against a
    /// responder minting a constant To-tag) seed no dialog and are not judged.
    #[test]
    fn out_of_dialog_transactions_reusing_a_tag_are_no_occasion() {
        let mut first =
            req(1_000, A, B, "OPTIONS", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1);
        first.call_id = "p1".to_string();
        let mut second =
            req(2_000, A, B, "OPTIONS", "sip:bob@h", 1, "z9hG4bK-2", "sip:alice@h", "at", Some("k"), 1);
        second.call_id = "p2".to_string();
        assert!(run(&DialogCallIdStable, &[first, second]).is_empty());
    }

    // ── cancel-request-uri / cancel-via-branch ──────────────────────────────

    #[test]
    fn a_cancel_matching_its_invite_is_compliant() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(2_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        ];
        assert!(charged(run(&CancelRequestUri, &msgs), A)
            .iter()
            .all(|x| matches!(x.decision, Decision::Compliant)));
        assert!(charged(run(&CancelViaBranch, &msgs), A)
            .iter()
            .all(|x| matches!(x.decision, Decision::Compliant)));
    }

    #[test]
    fn a_cancel_naming_another_uri_is_violated() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        ];
        let f = charged(run(&CancelRequestUri, &msgs), A);
        let Decision::Violated(Evidence::CancelUriDiverged { cancel_uri, invite_uri, .. }) =
            &f[0].decision
        else {
            panic!("cancel-uri evidence: {:?}", f[0].decision)
        };
        assert_eq!((cancel_uri.as_str(), invite_uri.as_str()), ("sip:eve@h", "sip:bob@h"));
    }

    /// A CANCEL on an unmatched branch is the BRANCH rule's finding; the URI
    /// rule has no INVITE to compare against and states nothing.
    #[test]
    fn a_cancel_on_a_foreign_branch_is_the_branch_rules_finding() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-other", "sip:alice@h", "at", None, 1),
        ];
        assert!(run(&CancelRequestUri, &msgs).is_empty());
        let f = charged(run(&CancelViaBranch, &msgs), A);
        let Decision::Violated(Evidence::CancelBranchUnmatched {
            cancel_branch, invite_branches, ..
        }) = &f[0].decision
        else {
            panic!("cancel-branch evidence: {:?}", f[0].decision)
        };
        assert_eq!(cancel_branch, "z9hG4bK-other");
        assert_eq!(invite_branches.as_slice(), ["z9hG4bK-i"]);
    }

    /// A re-INVITE opens a second transaction, and a CANCEL matching EITHER
    /// branch is fine.
    #[test]
    fn a_cancel_of_a_re_invite_branch_is_compliant() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(2_000, A, B, "INVITE", "sip:bob@h", 2, "z9hG4bK-i2", "sip:alice@h", "at", Some("bt"), 1),
            req(3_000, A, B, "CANCEL", "sip:bob@h", 2, "z9hG4bK-i2", "sip:alice@h", "at", Some("bt"), 1),
        ];
        let f = charged(run(&CancelViaBranch, &msgs), A);
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    /// Never having seen an INVITE, nothing can say the CANCEL is orphaned.
    #[test]
    fn a_cancel_without_any_invite_is_no_occasion() {
        let msgs = vec![req(
            1_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-c", "sip:alice@h", "at", None, 1,
        )];
        assert!(run(&CancelViaBranch, &msgs).is_empty());
    }

    // ── tag-consistency ─────────────────────────────────────────────────────

    #[test]
    fn a_final_keeping_the_provisional_tag_is_compliant() {
        let f = charged(run(&TagConsistency, &opened()), B);
        assert_eq!(f.len(), 1, "the final alone is the occasion: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_final_minting_a_fresh_tag_is_violated() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt2"), 1),
        ];
        let f = charged(run(&TagConsistency, &msgs), B);
        let Decision::Violated(Evidence::UasTagFlipped {
            final_tag, provisional_tags, status, ..
        }) = &f[0].decision
        else {
            panic!("tag-consistency evidence: {:?}", f[0].decision)
        };
        assert_eq!((*status, final_tag.as_str()), (200, "bt2"));
        assert_eq!(provisional_tags.as_slice(), ["bt"]);
    }

    /// A final on a transaction whose provisionals established no tag creates
    /// the dialog itself and owes nothing.
    #[test]
    fn a_final_without_a_prior_tagged_provisional_is_no_occasion() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        ];
        assert!(run(&TagConsistency, &msgs).is_empty());
    }

    /// §17.2.3: a CANCEL rides the INVITE's branch but forms its OWN server
    /// transaction, so the INVITE's provisional tag commits nothing on it — the
    /// answer to a CANCEL that matches no transaction states a tag of its own.
    #[test]
    fn a_cancel_answered_on_the_invite_branch_is_its_own_transaction() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            req(4_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(5_000, B, A, 481, 1, "CANCEL", "z9hG4bK-i", "at", Some("fallback"), 1),
        ];
        let f = charged(run(&TagConsistency, &msgs), B);
        assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
    }

    // ── no-to-tag-on-initial-request ────────────────────────────────────────

    #[test]
    fn an_initial_request_without_a_to_tag_is_compliant() {
        let f = charged(run(&NoToTagOnInitialRequest, &opened()), A);
        assert_eq!(f.len(), 1, "the INVITE alone opens one: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn an_initial_request_carrying_a_to_tag_is_violated() {
        let msgs = vec![req(
            1_000, A, B, "REFER", "sip:bob@h", 1, "z9hG4bK-r", "sip:alice@h", "at", Some("bogus"),
            1,
        )];
        let f = charged(run(&NoToTagOnInitialRequest, &msgs), A);
        let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, value, .. }) =
            &f[0].decision
        else {
            panic!("initial-to-tag evidence: {:?}", f[0].decision)
        };
        assert_eq!((on.as_str(), header.as_str(), value.as_str()), ("REFER", "To;tag", "bogus"));
    }

    /// Once ANY traffic has crossed on the Call-ID, the endpoint is inside a
    /// dialog and a To-tag on what it sends is legitimate.
    #[test]
    fn a_request_after_earlier_traffic_on_the_call_id_is_no_occasion() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(2_000, B, A, "NOTIFY", "sip:alice@h", 1, "z9hG4bK-n", "sip:bob@h", "bt", Some("at"), 1),
        ];
        let f = charged(run(&NoToTagOnInitialRequest, &msgs), B);
        assert!(f.is_empty(), "B had already taken the INVITE: {f:?}");
    }

    /// An intrinsically in-dialog verb is no occasion at any point — a lone BYE
    /// that opens a view is the dialog rules' business, not this one's.
    #[test]
    fn an_in_dialog_verb_is_no_occasion() {
        let msgs = vec![req(
            1_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1,
        )];
        assert!(run(&NoToTagOnInitialRequest, &msgs).is_empty());
    }

    // ── in-dialog-to-tag ────────────────────────────────────────────────────

    /// The caller's seat after INVITE / 200(tag=bt) / ACK.
    fn confirmed() -> Vec<Msg> {
        vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            req(3_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-k", "sip:alice@h", "at", Some("bt"), 1),
        ]
    }

    #[test]
    fn a_tagless_request_in_a_confirmed_dialog_is_violated() {
        let mut msgs = confirmed();
        msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", None, 1));
        let f = charged(run(&InDialogToTag, &msgs), A);
        let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
        let Decision::Violated(Evidence::RequiredHeaderAbsent { on, header, .. }) =
            &violated.decision
        else {
            panic!("in-dialog-to-tag evidence: {:?}", violated.decision)
        };
        assert_eq!((on.as_str(), header.as_str()), ("BYE", "To;tag"));
    }

    #[test]
    fn a_tagged_request_in_a_confirmed_dialog_is_compliant() {
        let mut msgs = confirmed();
        msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1));
        let f = charged(run(&InDialogToTag, &msgs), A);
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    /// The 2xx ACK is a fresh transaction the sender builds from dialog state —
    /// no response to echo, so the tag is its own to write.
    #[test]
    fn a_tagless_2xx_ack_is_violated() {
        let mut msgs = confirmed();
        msgs[2].to_tag = None;
        let f = charged(run(&InDialogToTag, &msgs), A);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{:?}", f[0].decision);
    }

    /// The ACK of a non-2xx rides the INVITE's branch and echoes that
    /// response's To — the responder's business, not the ACK sender's. A
    /// CANCEL likewise copies the INVITE it cancels.
    #[test]
    fn an_echoed_ack_or_cancel_is_no_occasion() {
        let mut msgs = confirmed();
        msgs.push(req(4_000, A, B, "INVITE", "sip:bob@h", 2, "z9hG4bK-r", "sip:alice@h", "at", Some("bt"), 1));
        msgs.push(resp(5_000, B, A, 488, 2, "INVITE", "z9hG4bK-r", "at", Some("bt"), 1));
        msgs.push(req(6_000, A, B, "ACK", "sip:bob@h", 2, "z9hG4bK-r", "sip:alice@h", "at", None, 1));
        msgs.push(req(7_000, A, B, "CANCEL", "sip:bob@h", 2, "z9hG4bK-r", "sip:alice@h", "at", None, 1));
        let violated = run(&InDialogToTag, &msgs).iter().filter(|x| x.violated()).count();
        assert_eq!(violated, 0, "the echoed requests are no occasion");
    }

    /// Same branch as the initial INVITE: still the dialog-establishing
    /// request, which is tag-less by §8.1.1.2.
    #[test]
    fn an_establishing_invite_retransmit_after_the_2xx_is_no_occasion() {
        let mut msgs = confirmed();
        msgs.push(req(4_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1));
        assert!(run(&InDialogToTag, &msgs).iter().all(|x| !x.violated()), "{msgs:?}");
    }

    /// Only a 2xx confirms: an unanswered INVITE creates no dialog, so the
    /// tag-less request that follows is correct.
    #[test]
    fn an_unconfirmed_dialog_opens_no_occasion() {
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 401, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
            req(3_000, A, B, "OPTIONS", "sip:bob@h", 2, "z9hG4bK-o", "sip:alice@h", "at", None, 1),
        ];
        assert!(run(&InDialogToTag, &msgs).is_empty());
    }

    /// The caller named no From-tag, so the dialog's remote tag is null and
    /// §12.2.1.1 requires the To tag parameter to be omitted: the answering
    /// side's tag-less BYE is the compliant one.
    #[test]
    fn a_peer_that_minted_no_tag_is_owed_nothing() {
        let mut invite =
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1);
        invite.from_tag = None;
        let msgs = vec![
            invite,
            resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            req(3_000, B, A, "BYE", "sip:alice@h", 1, "z9hG4bK-b", "sip:bob@h", "bt", None, 1),
        ];
        assert!(run(&InDialogToTag, &msgs).is_empty());
    }

    /// A vantage that carried the establishing INVITE BOTH ways forwards the
    /// originator's headers: the finding belongs to that originator.
    #[test]
    fn a_relaying_endpoint_judges_nothing() {
        let c = "10.0.0.3:5080";
        let msgs = vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
            req(1_500, B, c, "INVITE", "sip:bob@h", 1, "z9hG4bK-j", "sip:alice@h", "at", None, 1),
            resp(2_000, c, B, 200, 1, "INVITE", "z9hG4bK-j", "at", Some("ct"), 1),
            req(3_000, B, c, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", None, 1),
        ];
        assert!(charged(run(&InDialogToTag, &msgs), B).is_empty());
    }
}
