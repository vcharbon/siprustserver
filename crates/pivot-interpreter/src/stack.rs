//! The per-leg **UA stack**: the tier-1 the document never stores.
//!
//! A leg owns one symbolic dialog, and this is where its Call-ID, tags, CSeq
//! numbering, Via branches, Contact, remote target and route set live
//! (`PCAP2TEST_PIVOT_V3.md` §8, tier 1). Every message is composed by
//! `sip_message::generators` — the one place SIP is built in this tree — and the
//! interpreter contributes only what the document states.
//!
//! The stack also owns the AUTOMATICS an `auto` step marks (§6.3): 100 Trying,
//! the ACK to a final, and the CSeq an ACK reuses. The document says the flow
//! HAD them and the stack emits them; it never scripts one, and never reads an
//! auto step's `cseq` as a CSeq to emit.
//!
//! **Identity is per RUN, not per document.** A plan is compiled once and run
//! many times, concurrently; a Call-ID or a From-tag derived from the leg id
//! alone would be identical in every instance, which RFC 3261 §8.1.1.4 forbids
//! and which lets a rerun fold into the system's prior dialog state. The run's
//! nonce rides both, with the leg id kept in front so a trace still reads.

mod error;
mod forks;
mod learn;
mod request;
mod respond;

pub use error::StackError;
pub(crate) use respond::reason_for;

use std::collections::BTreeSet;
use std::net::SocketAddr;

use sip_message::generators::StackDialog;
use sip_message::header::{self, MediaType, Uri, Via};
use sip_message::{Method, SipHeader, SipRequest, SipResponse, SipStr, TemplateHeader};

/// The tier-2 addresses a dialog-opening request carries: the lane composed
/// them, and the stack regenerates every tier-1 header around them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addresses<'a> {
    pub ruri: &'a str,
    pub from: &'a str,
    pub to: &'a str,
}

/// What a scripted response states: its status line, the transaction it
/// answers, and the early dialog it answers under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer<'a> {
    pub status: u16,
    pub reason: &'a str,
    /// The CSeq method of the request it answers; `None` answers the newest.
    pub cseq_method: Option<&'a str>,
    /// The fork it rings or answers under (§6.1), where the step names one.
    pub early_tag: Option<&'a str>,
}

/// A reliable provisional as its acknowledgement needs it: RFC 3262 §7.2's
/// `RAck` is the provisional's `RSeq` plus the CSeq of the request it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReliableProvisional {
    pub rseq: u32,
    pub cseq: u32,
    pub cseq_method: Method,
    /// The early dialog it rang under, as its To-tag names it.
    pub to_tag: Option<String>,
}

impl ReliableProvisional {
    /// The `RAck` header value that acknowledges it.
    pub fn rack(&self) -> String {
        format!("{} {} {}", self.rseq, self.cseq, self.cseq_method)
    }
}

/// ONE early dialog of a leg (RFC 3261 §12.1.2), keyed everywhere by the To-tag
/// the UAS side minted for it.
///
/// It carries a whole dialog view because that is what an early-dialog request
/// is composed against: RFC 3311 §5.1's UPDATE takes its remote tag, remote
/// target and route set from the provisional that opened THIS fork, and numbers
/// its CSeq in this dialog's own space (RFC 3261 §12.2.1.1).
#[derive(Debug, Clone)]
struct EarlyDialog {
    dialog: StackDialog,
    /// The RSeq of the last reliable provisional that rode it, where one has.
    rseq: Option<u32>,
    /// The CSeq of the INVITE whose provisional opened it.
    invite: u32,
    /// Whether a non-2xx final to that INVITE ended it (RFC 3261 §12.3). An
    /// ended dialog rings no more, and keeps what it held for a request a
    /// peer still sends into it.
    ended: bool,
}

/// One leg's dialog and transaction state.
pub struct LegStack {
    pub leg: String,
    /// The socket this leg's actor rides.
    pub addr: SocketAddr,
    /// Where an out-of-dialog request is addressed before a route set exists.
    pub route_target: SocketAddr,
    dialog: StackDialog,
    /// Whether a 2xx to a dialog-creating request, sent or received, opened
    /// the leg's dialog (RFC 3261 §12.1, RFC 6665 §4.1.2). No other final
    /// opens one.
    has_dialog: bool,
    /// Whether an INVITE 2xx has CONFIRMED the leg's dialog (RFC 3261 §13).
    /// Distinct from `has_dialog`, which a SUBSCRIBE's 2xx opens too: only
    /// confirmation ends the early phase, adopts the answering fork's sequence,
    /// and retires the fork ride a PRACK takes before it.
    confirmed: bool,
    /// The INVITEs this side sent, in send order, for their ACK and CANCEL.
    sent_invites: Vec<SipRequest>,
    /// The CSeqs among them this leg has already ACKed. RFC 3261 §14.1 leaves
    /// one INVITE outstanding per dialog, so a compliant peer's leg has at most
    /// one un-ACKed entry; a capture of a peer that pipelined a second
    /// re-INVITE before ACKing the first's 2xx has two, and each ACK discharges
    /// the transaction its own RESPONSE names.
    acked_invites: BTreeSet<u32>,
    /// Requests received on this leg, newest last: a response answers one.
    received: Vec<SipRequest>,
    /// The CSeqs among them this leg has already answered with a FINAL. RFC
    /// 3261 §17.2.1 ends a server transaction at its final, so a response
    /// composed against one of these would be a duplicate the far side
    /// discards (§17.1.3) while the transaction it was owed stays open.
    finalled: BTreeSet<u32>,
    /// Reliable provisionals this leg RECEIVED and has not PRACKed, oldest
    /// first: what an `RAck` is built out of (RFC 3262 §7.2).
    unacknowledged: Vec<ReliableProvisional>,
    /// The RSeq of the last reliable provisional this leg SIGHTED, sent or
    /// received — what `${leg:<id>.rseq}` publishes. It is one number per leg,
    /// so a leg ringing several forks at once reads the newest.
    last_rseq: Option<u32>,
    /// The early dialogs this leg rang, still ringing or ended, by the To-tag
    /// each answers under, in the order they rang.
    early: Vec<(String, EarlyDialog)>,
    /// This run's identity nonce. It rides the Via branch as well as the
    /// Call-ID and the tag: RFC 3261 §8.1.1.7 requires a branch unique across
    /// TIME, and §17.2.3 matches a server transaction on branch + sent-by +
    /// method WITHOUT the Call-ID — so two runs sharing a socket would merge
    /// transactions if only the Call-ID varied.
    nonce: String,
    /// Branch and tag counter — one id source per leg, so a trace reads.
    ids: u64,
    /// How many times this leg's party refreshed its remote target
    /// ([`LegStack::refresh_target`]); its Contact user part names the count.
    target_refreshes: u32,
}

/// The dialog identity a leg's out-of-dialog requests carry: its Call-ID and
/// its own tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogIdentity {
    pub call_id: String,
    pub local_tag: String,
}

/// The identity leg `leg` dials under in the run seeded by `nonce`.
pub fn dialog_identity(leg: &str, nonce: &str) -> DialogIdentity {
    DialogIdentity {
        call_id: format!("{leg}-{nonce}@pivot.invalid"),
        local_tag: format!("{leg}-{nonce}-tag"),
    }
}

impl LegStack {
    /// A leg that has not spoken yet. `local_uri`/`remote_uri` are the
    /// addresses its From and To carry; the lane composes them.
    pub fn new(
        leg: impl Into<String>,
        addr: SocketAddr,
        route_target: SocketAddr,
        run_nonce: &str,
        local_uri: impl Into<String>,
        remote_uri: impl Into<String>,
    ) -> Self {
        let leg = leg.into();
        let local_uri = local_uri.into();
        let remote_uri = remote_uri.into();
        let identity = dialog_identity(&leg, run_nonce);
        LegStack {
            dialog: StackDialog {
                call_id: identity.call_id,
                local_tag: identity.local_tag,
                remote_tag: String::new(),
                local_uri,
                remote_uri: remote_uri.clone(),
                remote_target: remote_uri,
                local_cseq: 0,
                route_set: Vec::new(),
            },
            leg,
            addr,
            route_target,
            nonce: run_nonce.to_string(),
            has_dialog: false,
            confirmed: false,
            sent_invites: Vec::new(),
            acked_invites: BTreeSet::new(),
            received: Vec::new(),
            finalled: BTreeSet::new(),
            unacknowledged: Vec::new(),
            last_rseq: None,
            early: Vec::new(),
            ids: 0,
            target_refreshes: 0,
        }
    }

    /// Refresh this leg's remote target for the `n`-th time (RFC 3261
    /// §12.1.2, §12.2.1.2): from the next message on, its Contact names a user
    /// part of its own. The host and port stay the bound socket's.
    pub fn refresh_target(&mut self, n: u32) {
        self.target_refreshes = n;
    }

    /// Whether a 2xx to a dialog-creating request opened the leg's dialog.
    #[cfg(test)]
    pub fn has_dialog(&self) -> bool {
        self.has_dialog
    }

    /// Whether a request this leg sends now stands outside any dialog: no 2xx
    /// opened one, the leg sent no INVITE, and it rang no early dialog
    /// (RFC 3261 §12.1).
    pub fn outside_any_dialog(&self) -> bool {
        !self.has_dialog && self.sent_invites.is_empty() && self.early.is_empty()
    }

    /// Whether an INVITE 2xx has confirmed the leg's dialog (RFC 3261 §13).
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// The INVITE this leg sent most recently, whether or not it is ACKed.
    pub fn sent_invite(&self) -> Option<&SipRequest> {
        self.sent_invites.last()
    }

    /// The CSeqs of the INVITEs this leg sent and has not ACKed, in send order:
    /// the transactions still owed one.
    pub fn outstanding_invites(&self) -> impl Iterator<Item = u32> + '_ {
        self.sent_invites
            .iter()
            .map(|invite| invite.cseq().seq())
            .filter(|seq| !self.acked_invites.contains(seq))
    }

    /// The CSeqs of every INVITE this leg sent, in send order, ACKed or not: the
    /// transactions a response arriving on this leg can be answering.
    pub fn sent_invite_cseqs(&self) -> impl Iterator<Item = u32> + '_ {
        self.sent_invites.iter().map(|invite| invite.cseq().seq())
    }

    /// The Call-ID this leg carries: its [`dialog_identity`]'s, or the one the
    /// last INVITE it took names.
    pub fn call_id(&self) -> &str {
        &self.dialog.call_id
    }

    fn next_id(&mut self) -> u64 {
        self.ids += 1;
        self.ids
    }

    fn via(&mut self) -> Via {
        let id = self.next_id();
        let branch = format!("z9hG4bK-{}-{}-{id}", self.leg, self.nonce);
        Via::udp(SipStr::owned(&self.addr.ip().to_string()), self.addr.port())
            .with_branch(SipStr::owned(&branch))
    }

    fn contact(&self) -> header::Contact {
        let user = match self.target_refreshes {
            0 => self.leg.clone(),
            n => format!("{}-{n}", self.leg),
        };
        header::Contact::from_uri(
            Uri::sip_user(SipStr::owned(&user), SipStr::owned(&self.addr.ip().to_string()))
                .with_port(self.addr.port()),
        )
    }

    /// Where this leg's next request goes: the lane's route target.
    pub fn next_hop(&self) -> SocketAddr {
        self.route_target
    }

    /// Where a response to the request `cseq_method` names goes: the topmost
    /// Via's sent-by, per RFC 3261 §18.2.2. Absent where the leg has answered
    /// nothing yet or the Via names no address this run can reach.
    pub fn response_target(&self, cseq_method: Option<&str>) -> Option<SocketAddr> {
        let wanted = cseq_method.map(Method::from_wire);
        let request = self
            .received
            .iter()
            .rev()
            .find(|r| wanted.as_ref().is_none_or(|m| r.cseq().method() == m))?;
        via_target(request)
    }

    /// The reliable provisionals this leg has received and not yet PRACKed.
    #[cfg(test)]
    pub fn unacknowledged(&self) -> &[ReliableProvisional] {
        &self.unacknowledged
    }

    /// The RSeq this leg last put on the wire or read off it.
    pub fn rseq(&self) -> Option<u32> {
        self.last_rseq
    }

    /// The CSeq the peer last used on this leg.
    pub fn remote_cseq(&self) -> Option<u32> {
        self.received.last().map(|r| r.cseq().seq())
    }

    /// The CSeq this side last used.
    pub fn local_cseq(&self) -> Option<u32> {
        (self.dialog.local_cseq > 0).then_some(self.dialog.local_cseq)
    }

    pub fn local_tag(&self) -> &str {
        &self.dialog.local_tag
    }

    pub fn remote_tag(&self) -> Option<&str> {
        (!self.dialog.remote_tag.is_empty()).then_some(self.dialog.remote_tag.as_str())
    }

    pub fn remote_target(&self) -> &str {
        &self.dialog.remote_target
    }

    pub fn route_set(&self) -> &[String] {
        &self.dialog.route_set
    }
}

fn media_type(text: &str) -> MediaType {
    use sip_message::header::HeaderValue;
    MediaType::parse(&SipStr::owned(text)).unwrap_or_else(|_| MediaType::new(SipStr::owned(text)))
}

/// Where a response to `request` goes: the topmost Via's sent-by, per RFC 3261
/// §18.2.2. Absent where the Via names no address this run can reach.
pub(crate) fn via_target(request: &SipRequest) -> Option<SocketAddr> {
    let via = request.top_via();
    let (host, port) = via.host_port();
    let host = via.received().unwrap_or(host);
    format!("{host}:{port}").parse().ok()
}

/// Whether a provisional is reliable (RFC 3262 §3: `Require: 100rel`). Header
/// reading is `sip-message`'s.
pub(crate) fn reliably(response: &SipResponse) -> bool {
    response
        .header::<header::Require>()
        .and_then(Result::ok)
        .is_some_and(|tokens| tokens.contains("100rel"))
}

/// Whether a request creates a dialog when a 2xx answers it: an INVITE or a
/// SUBSCRIBE (RFC 3261 §12.1, RFC 6665 §4.1.2).
pub(crate) fn creates_dialog(method: &Method) -> bool {
    matches!(method, Method::Invite | Method::Subscribe)
}

/// The `RSeq` a response carries, where it carries a readable one.
pub(crate) fn rseq_of(response: &SipResponse) -> Option<u32> {
    response.header::<header::RSeq>().and_then(Result::ok).map(|r| r.value())
}

/// The document's frozen headers, as generator input.
fn frozen(headers: &[TemplateHeader]) -> Vec<SipHeader> {
    headers.iter().map(|h| SipHeader::new(h.name.clone(), h.value.clone())).collect()
}

#[cfg(test)]
mod tests;
