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

use std::collections::BTreeSet;
use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_2xx, generate_ack_for_non_2xx, generate_cancel, generate_in_dialog_request,
    generate_out_of_dialog_request, generate_response, GenerateAckFor2xxOpts,
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, InviteClientTransactionHandle, OutOfDialogMethod, StackDialog,
};
use sip_message::header::{self, MediaType, NameAddr, Uri, Via};
use sip_message::{Method, SipHeader, SipRequest, SipResponse, SipStr, TemplateHeader};

/// The tier-2 addresses a dialog-opening request carries: the lane composed
/// them, and the stack regenerates every tier-1 header around them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addresses<'a> {
    pub ruri: &'a str,
    pub from: &'a str,
    pub to: &'a str,
}

/// Why the stack could not compose a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackError {
    /// An in-dialog request on a leg with no dialog yet.
    NoDialog { leg: String, method: String },
    /// A response with no request to answer.
    NoRequest { leg: String, status: u16 },
    /// An ACK or CANCEL with no INVITE to scope it to.
    NoInvite { leg: String, method: String },
    /// A method this stack has no recipe for.
    UnsupportedMethod { leg: String, method: String },
    /// A reliable provisional composed without the `RSeq` RFC 3262 §3 requires.
    NoRSeq { leg: String, status: u16 },
    /// A PRACK on a leg that has received no reliable provisional to
    /// acknowledge: RFC 3262 §7.2 builds `RAck` out of one, and there is none.
    NoReliableProvisional { leg: String },
    /// An early-dialog request on a leg ringing SEVERAL forks, naming none of
    /// them. Picking one would be the inference §14 forbids.
    EarlyDialogAmbiguous { leg: String, method: String, dialogs: usize },
    /// An early-dialog request naming a fork this leg has not rung.
    EarlyDialogUnknown { leg: String, method: String, early: String },
    /// A `cseq-override` (§11) on the ACK to a non-2xx final. That ACK is
    /// composed by the INVITE client transaction it rides (RFC 3261 §17.1.1.3),
    /// so its number is not the core's to state.
    CseqOverrideOnAbsorbedAck { leg: String },
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StackError::NoDialog { leg, method } => {
                write!(f, "leg {leg}: {method} is in-dialog and the leg has no dialog yet")
            }
            StackError::NoRequest { leg, status } => {
                write!(f, "leg {leg}: {status} answers a request the leg never received")
            }
            StackError::NoInvite { leg, method } => {
                write!(f, "leg {leg}: {method} needs an INVITE transaction and there is none")
            }
            StackError::UnsupportedMethod { leg, method } => {
                write!(f, "leg {leg}: this stack has no recipe for {method}")
            }
            StackError::NoRSeq { leg, status } => write!(
                f,
                "leg {leg}: the {status} requires 100rel and states no RSeq, and none is invented"
            ),
            StackError::NoReliableProvisional { leg } => write!(
                f,
                "leg {leg}: PRACK acknowledges a reliable provisional and this leg received none"
            ),
            StackError::EarlyDialogAmbiguous { leg, method, dialogs } => write!(
                f,
                "leg {leg}: {method} rides an early dialog and the leg has {dialogs} of them; \
                 state which with `early`"
            ),
            StackError::EarlyDialogUnknown { leg, method, early } => write!(
                f,
                "leg {leg}: {method} rides early dialog {early:?}, which this leg has not rung"
            ),
            StackError::CseqOverrideOnAbsorbedAck { leg } => write!(
                f,
                "leg {leg}: the ACK to a non-2xx final is composed by the INVITE client \
                 transaction and its CSeq is not the core's to override"
            ),
        }
    }
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
}

/// One leg's dialog and transaction state.
pub struct LegStack {
    pub leg: String,
    /// The socket this leg's actor rides.
    pub addr: SocketAddr,
    /// Where an out-of-dialog request is addressed before a route set exists.
    pub route_target: SocketAddr,
    dialog: StackDialog,
    /// Whether the leg has learned enough to send in-dialog.
    has_dialog: bool,
    /// Whether an INVITE 2xx has CONFIRMED the leg's dialog (RFC 3261 §13).
    /// Distinct from `has_dialog`, which any final answer opens: only
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
    /// The forks this leg is ringing, by the To-tag each answers under, in the
    /// order they rang.
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

    pub fn has_dialog(&self) -> bool {
        self.has_dialog
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

    /// Where this leg's next request goes. The lane's route target: an
    /// out-of-dialog request is addressed at the system under test, and an
    /// in-dialog one carries its Route set so the R-URI reaches the target
    /// through the same hop.
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
        let via = request.top_via();
        let (host, port) = via.host_port();
        let host = via.received().unwrap_or(host);
        format!("{host}:{port}").parse().ok()
    }

    /// Compose the dialog-opening request of an originating leg. The R-URI,
    /// From and To are the lane's tier-2 values; everything else is tier 1.
    ///
    /// `cseq` is a `cseq-override` (§11): it BECOMES the leg's sequence number,
    /// because RFC 3261 §12.2.1.1 numbers every later request from the dialog's
    /// own counter and the transaction this request opens is identified by what
    /// went out.
    pub fn out_of_dialog(
        &mut self,
        method: Method,
        addresses: &Addresses<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let (ruri, from, to) = (addresses.ruri, addresses.from, addresses.to);
        let verb = out_of_dialog_method(&method).ok_or_else(|| StackError::UnsupportedMethod {
            leg: self.leg.clone(),
            method: method.to_string(),
        })?;
        self.dialog.local_uri = from.to_string();
        self.dialog.remote_uri = to.to_string();
        self.dialog.local_cseq = cseq.unwrap_or(self.dialog.local_cseq + 1);
        let via = self.via();
        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of(ruri)),
            call_id: self.dialog.call_id.clone(),
            from: Some(
                header::From::new(addr_of(from)).with_tag(SipStr::owned(&self.dialog.local_tag)),
            ),
            to: Some(header::To::new(addr_of(to))),
            cseq: self.dialog.local_cseq,
            via: Some(via),
            contact: Some(self.contact()),
            max_forwards: None,
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: frozen(headers),
        };
        let request = generate_out_of_dialog_request(verb, &opts);
        if method == Method::Invite {
            self.sent_invites.push(request.clone());
        }
        Ok(request)
    }

    /// Compose an in-dialog request. `cseq` is a `cseq-override` (§11) and
    /// becomes the dialog's sequence number, as it does on a dialog-opening
    /// request. Before any final it rides the leg's only early dialog
    /// ([`in_dialog_on`](Self::in_dialog_on)).
    pub fn in_dialog(
        &mut self,
        method: Method,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        self.in_dialog_on(None, method, headers, body, content_type, cseq)
    }

    /// Whether the leg rings an early dialog it can address a request on: no
    /// final has opened a dialog yet, and a provisional under a To-tag opened
    /// one on either side (RFC 3261 §12.1).
    pub fn rings_early(&self) -> bool {
        !self.has_dialog && !self.early.is_empty()
    }

    /// Compose an in-dialog request on the dialog the leg has: the one a final
    /// opened, else the early dialog `early` names — the leg's only one where
    /// it names none (RFC 3261 §12.1.1: a UAS's provisional under a To-tag
    /// opens an early dialog on its side too, and a BYE may ride it, §15).
    pub fn in_dialog_on(
        &mut self,
        early: Option<&str>,
        method: Method,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let verb = in_dialog_method(&method).ok_or_else(|| StackError::UnsupportedMethod {
            leg: self.leg.clone(),
            method: method.to_string(),
        })?;
        let fork = if self.has_dialog { None } else { Some(self.early_tag_for(early, &method)?) };
        let dialog = match &fork {
            None => self.dialog.clone(),
            Some(tag) => self.early_dialog(tag).map(|e| e.dialog.clone()).ok_or_else(|| {
                StackError::EarlyDialogUnknown {
                    leg: self.leg.clone(),
                    method: method.to_string(),
                    early: tag.clone(),
                }
            })?,
        };
        let via = self.via();
        let opts = GenerateInDialogRequestOpts {
            via: Some(via),
            contact: Some(self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: frozen(headers),
            cseq,
            ..Default::default()
        };
        let result = generate_in_dialog_request(verb, &dialog, &opts);
        match fork {
            None => self.dialog = result.dialog,
            Some(tag) => {
                if let Some((_, early)) = self.early.iter_mut().find(|(known, _)| *known == tag) {
                    early.dialog = result.dialog;
                }
            }
        }
        if method == Method::Invite {
            self.sent_invites.push(result.request.clone());
        }
        Ok(result.request)
    }

    /// The early dialog a request rides: the one `early` names, else the
    /// leg's only one. None and several are refusals: which dialog a request
    /// rides decides which endpoint it reaches, and §14 has the tool infer
    /// nothing.
    fn early_tag_for(&self, early: Option<&str>, method: &Method) -> Result<String, StackError> {
        if let Some(named) = early {
            return Ok(named.to_string());
        }
        match self.early.len() {
            0 => Err(StackError::NoDialog { leg: self.leg.clone(), method: method.to_string() }),
            1 => Ok(self.early[0].0.clone()),
            dialogs => Err(StackError::EarlyDialogAmbiguous {
                leg: self.leg.clone(),
                method: method.to_string(),
                dialogs,
            }),
        }
    }

    /// Compose a response to the newest received request whose CSeq method is
    /// `cseq_method`, or to the newest received request when none is named.
    ///
    /// **Which dialog it answers under.** A request already carrying a To-tag
    /// names its dialog and the response repeats it (RFC 3261 §12.1.1);
    /// otherwise `early_tag` — the fork the step rides (§6.1) — answers, and a
    /// leg with no fork answers under its own tag. A 2xx INVITE final under a
    /// fork's tag is the dialog that CONFIRMED, so the leg adopts it: the
    /// requests that follow ride the dialog that was answered, not the one the
    /// leg opened with.
    ///
    /// **What it advertises.** Only what the document states. A generated
    /// response relays no capability set of its own, because RFC 3261 §20.5 /
    /// §20.37 read an absent `Allow` / `Supported` as "no information given" —
    /// the only truthful thing to say about a set no capture held.
    pub fn respond(
        &mut self,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<SipResponse, StackError> {
        self.respond_selected(answer, headers, body, content_type, |_| true)
    }

    /// [`respond`](Self::respond) to ONE named server transaction: the request
    /// with this CSeq number on the dialog `to_tag` names — two forks number
    /// their first in-dialog request alike, so the tag is part of the name.
    pub fn respond_to(
        &mut self,
        cseq: u32,
        to_tag: Option<&str>,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<SipResponse, StackError> {
        self.respond_selected(answer, headers, body, content_type, |r| {
            r.cseq().seq() == cseq && r.to().tag() == to_tag
        })
    }

    fn respond_selected(
        &mut self,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        selected: impl Fn(&SipRequest) -> bool,
    ) -> Result<SipResponse, StackError> {
        let Answer { status, reason, cseq_method, early_tag } = *answer;
        let wanted = cseq_method.map(Method::from_wire);
        let matching = || {
            self.received
                .iter()
                .rev()
                .filter(|r| wanted.as_ref().is_none_or(|m| r.cseq().method() == m))
                .filter(|r| selected(r))
        };
        // The newest transaction of that method still OPEN, and only then the
        // newest at all. A capture may answer two outstanding transactions of
        // one method out of order — a PRACK ladder is where it happens — and
        // recency alone would stamp the second response with the transaction
        // the first already ended, leaving the older one unanswered for the
        // whole run.
        let request = matching()
            .find(|r| !self.finalled.contains(&r.cseq().seq()))
            .or_else(|| matching().next())
            .cloned()
            .ok_or(StackError::NoRequest { leg: self.leg.clone(), status })?;
        let states_contact =
            sip_message::generators::response_states_contact(request.cseq().method(), status);
        let mut extra = frozen(headers);
        let to_tag = request
            .to()
            .tag()
            .map(str::to_string)
            .or_else(|| early_tag.map(str::to_string))
            .unwrap_or_else(|| self.dialog.local_tag.clone());
        if (101..200).contains(&status) && *request.cseq().method() == Method::Invite {
            self.pace_rseq(&to_tag, &mut extra);
        }
        let opts = GenerateResponseOpts {
            to_tag: Some(to_tag.clone()),
            contact: states_contact.then(|| self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: extra,
            incoming_source: None,
        };
        let response = generate_response(&request, status, reason, &opts);
        let mut rseq = None;
        if (101..200).contains(&status) && reliably(&response) {
            rseq = Some(
                rseq_of(&response).ok_or(StackError::NoRSeq { leg: self.leg.clone(), status })?,
            );
            self.last_rseq = rseq;
        }
        // A provisional under a To-tag opens an early dialog on this side
        // (RFC 3261 §12.1.1): the fork answers under that tag, on the dialog
        // the INVITE established, in its own CSeq space.
        if (101..200).contains(&status) && *request.cseq().method() == Method::Invite {
            let fork =
                StackDialog { local_tag: to_tag.clone(), local_cseq: 0, ..self.dialog.clone() };
            self.ring_early(&to_tag, fork, rseq);
        }
        if status >= 200 {
            self.finalled.insert(request.cseq().seq());
        }
        if status >= 200 && request.cseq().method() == Method::Invite {
            self.has_dialog = true;
            if (200..300).contains(&status) {
                // The fork that answers IS the dialog that confirmed, so the
                // leg takes its tag and continues that fork's OWN sequence
                // (RFC 3261 §12.2.1.1) — never a number another fork burned.
                let fork = self.early_dialog(&to_tag).map(|e| e.dialog.local_cseq);
                self.dialog.local_tag = to_tag;
                if !self.confirmed {
                    self.confirmed = true;
                    self.dialog.local_cseq = fork.unwrap_or(self.dialog.local_cseq);
                } else {
                    self.dialog.local_cseq = self.dialog.local_cseq.max(fork.unwrap_or(0));
                }
            }
        }
        Ok(response)
    }

    /// Number the `RSeq` the document states into this fork's own sequence.
    ///
    /// RFC 3262 §3 gives every early dialog ONE space: its first reliable
    /// provisional seeds it and each later one is greater by exactly one, so a
    /// document that states an unrelated number on the second is renumbered
    /// onto the space it rides.
    /// A number the document repeats retransmits its provisional and stands.
    fn pace_rseq(&self, tag: &str, extra: &mut [SipHeader]) {
        let Some(last) = self.early_rseq(tag) else { return };
        for header in extra.iter_mut().filter(|h| sip_message::HeaderName::RSeq.matches(&h.name)) {
            let stated = header.value.as_str().trim().parse::<u32>().ok();
            if stated.is_some_and(|stated| stated != last) {
                header.value = SipStr::owned(&last.saturating_add(1).to_string());
            }
        }
    }

    /// Open or refresh the early dialog `tag` names. A later provisional on the
    /// same fork refreshes the target and route set it carried; the dialog's own
    /// sequence number stands.
    fn ring_early(&mut self, tag: &str, dialog: StackDialog, rseq: Option<u32>) {
        match self.early.iter_mut().find(|(known, _)| known == tag) {
            Some((_, early)) => {
                early.dialog = StackDialog { local_cseq: early.dialog.local_cseq, ..dialog };
                if rseq.is_some() {
                    early.rseq = rseq;
                }
            }
            None => self.early.push((tag.to_string(), EarlyDialog { dialog, rseq })),
        }
    }

    fn early_dialog(&self, tag: &str) -> Option<&EarlyDialog> {
        self.early.iter().find(|(known, _)| known == tag).map(|(_, early)| early)
    }

    /// The RSeq of the last reliable provisional that rode ONE fork — what
    /// `${early:<id>.rseq}` publishes, where `${leg:<id>.rseq}` has room for
    /// only the newest of them.
    pub fn early_rseq(&self, tag: &str) -> Option<u32> {
        self.early_dialog(tag).and_then(|early| early.rseq)
    }

    /// The forks this leg is ringing, by the To-tag each answers under.
    #[cfg(test)]
    pub fn early_tags(&self) -> impl Iterator<Item = &str> {
        self.early.iter().map(|(tag, _)| tag.as_str())
    }

    /// The UPDATE this leg sends (RFC 3311 §5.1).
    ///
    /// An UPDATE rides a CONFIRMED dialog where the leg has one. Before
    /// confirmation it rides an EARLY dialog, taking that fork's remote tag,
    /// remote target and route set: `early` names which, and a leg ringing one
    /// fork needs no name. Several forks and no name is a refusal — which
    /// dialog an UPDATE rides decides which endpoint it reaches, and §14 has the
    /// tool infer nothing.
    pub fn update(
        &mut self,
        early: Option<&str>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        self.in_dialog_on(early, Method::Update, headers, body, content_type, cseq)
    }

    /// The PRACK this leg owes a reliable provisional it received (RFC 3262
    /// §7.2): the oldest un-acknowledged one OF THE FORK the step names, or the
    /// oldest the leg holds where it names none. The `RAck` is composed from
    /// that provisional — never from a number the document repeats — unless the
    /// step froze one, which wins as every frozen header does.
    ///
    /// It rides the EARLY dialog the acknowledged provisional opened, exactly
    /// as [`LegStack::update`] rides the fork it names: composed from that
    /// fork's dialog and numbered in its own sequence (RFC 3261 §12.2.1.1), so
    /// the To-tag and the RAck name ONE dialog. A PRACK is in-dialog on a
    /// dialog that is not confirmed yet; once the leg's dialog confirms, it
    /// rides that.
    pub fn prack(
        &mut self,
        early: Option<&str>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        if let Some(named) = early {
            if !self.confirmed && self.early_dialog(named).is_none() {
                return Err(StackError::EarlyDialogUnknown {
                    leg: self.leg.clone(),
                    method: Method::Prack.to_string(),
                    early: named.to_string(),
                });
            }
        }
        let chosen = match early {
            Some(named) => {
                self.unacknowledged.iter().position(|p| p.to_tag.as_deref() == Some(named))
            }
            None => (!self.unacknowledged.is_empty()).then_some(0),
        };
        let mut extra = frozen(headers);
        if !extra.iter().any(|h| sip_message::HeaderName::RAck.matches(&h.name)) {
            let provisional = chosen
                .map(|at| &self.unacknowledged[at])
                .ok_or(StackError::NoReliableProvisional { leg: self.leg.clone() })?;
            extra.push(SipHeader::new("RAck", provisional.rack()));
        }
        let fork = (!self.confirmed)
            .then(|| match early {
                Some(named) => Some(named.to_string()),
                None => chosen.and_then(|at| self.unacknowledged[at].to_tag.clone()),
            })
            .flatten()
            .filter(|tag| self.early_dialog(tag).is_some());
        if let Some(at) = chosen {
            self.unacknowledged.remove(at);
        }
        let via = self.via();
        let opts = GenerateInDialogRequestOpts {
            via: Some(via),
            contact: Some(self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: extra,
            cseq,
            ..Default::default()
        };
        let dialog = fork
            .as_deref()
            .and_then(|tag| self.early_dialog(tag))
            .map(|e| e.dialog.clone())
            .unwrap_or_else(|| self.dialog.clone());
        let result = generate_in_dialog_request(InDialogMethod::Prack, &dialog, &opts);
        match fork {
            Some(tag) => {
                if let Some((_, early)) = self.early.iter_mut().find(|(known, _)| *known == tag) {
                    early.dialog = result.dialog;
                }
            }
            None => self.dialog = result.dialog,
        }
        Ok(result.request)
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

    /// The ACK the stack owes a final response — the automatic an `auto` step
    /// marks. A 2xx ACK is a new transaction on the dialog; a non-2xx ACK rides
    /// the INVITE's own branch (RFC 3261 §17.1.1.3).
    ///
    /// The RESPONSE names the transaction: an ACK carries the CSeq of the
    /// INVITE the final answered, never the number of whichever INVITE the leg
    /// happened to send last. The two differ only where a captured peer broke
    /// RFC 3261 §14.1 and left two INVITEs outstanding at once — and there,
    /// composing against the newest silently re-ACKs one transaction and leaves
    /// the other's 2xx unacknowledged for the life of the dialog.
    ///
    /// The step's stored content rides it: the frozen headers on both arms, and
    /// the BODY on the 2xx arm alone, which is where a delayed offer's answer
    /// travels (RFC 3261 §13.2.1). A non-2xx ACK is absorbed by the INVITE
    /// transaction and reaches no TU that could read one, so the document may
    /// not store one there (§6.3) and this arm carries none.
    ///
    /// `cseq` is a `cseq-override` (§11) and changes ONLY the number the wire
    /// carries: the INVITE this ACK acknowledges is still found by the
    /// response's own CSeq, and is still the transaction the emission rides.
    /// The 2xx arm alone honours it — a non-2xx ACK is composed by the INVITE
    /// client transaction (RFC 3261 §17.1.1.3), whose number is not the core's
    /// to state, so an override there is refused rather than dropped.
    pub fn ack_for(
        &mut self,
        response: &SipResponse,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let invite = self
            .sent_invites
            .iter()
            .rev()
            .find(|invite| invite.cseq().seq() == response.cseq().seq())
            .or_else(|| self.sent_invites.last())
            .cloned()
            .ok_or_else(|| StackError::NoInvite { leg: self.leg.clone(), method: "ACK".into() })?;
        self.acked_invites.insert(invite.cseq().seq());
        if response.status() < 300 {
            let txn = InviteClientTransactionHandle { original_invite: invite };
            let via = self.via();
            let opts = GenerateAckFor2xxOpts {
                via: Some(via),
                body,
                content_type: content_type.as_deref().map(media_type),
                extra_headers: frozen(headers),
                cseq,
                ..Default::default()
            };
            // The ACK rides the dialog the 2xx created, under ITS To-tag (RFC
            // 3261 §13.2.2.4) — not whatever tag the leg learned since.
            let dialog = match response.to().tag() {
                Some(tag) => StackDialog { remote_tag: tag.to_string(), ..self.dialog.clone() },
                None => self.dialog.clone(),
            };
            Ok(generate_ack_for_2xx(Some(&txn), &dialog, &opts))
        } else if cseq.is_some() {
            Err(StackError::CseqOverrideOnAbsorbedAck { leg: self.leg.clone() })
        } else {
            Ok(generate_ack_for_non_2xx(&invite, response, &frozen(headers)))
        }
    }

    /// The CANCEL for this leg's pending INVITE.
    pub fn cancel(&mut self, headers: &[TemplateHeader]) -> Result<SipRequest, StackError> {
        let invite = self.sent_invite().cloned().ok_or_else(|| StackError::NoInvite {
            leg: self.leg.clone(),
            method: "CANCEL".into(),
        })?;
        let txn = InviteClientTransactionHandle { original_invite: invite };
        Ok(generate_cancel(&txn, &frozen(headers)))
    }

    /// Learn the dialog facts a response carries: the remote tag and the route
    /// set from a 1xx/2xx to a dialog-creating request alone (RFC 3261
    /// §12.1.2), the remote target from any non-final or 2xx (§12.2.1.2). A
    /// final to a CANCEL or a BYE, or a non-2xx final, states no dialog fact
    /// and leaves the dialog as it is; so does, once a 2xx confirmed the
    /// dialog, a response under another fork's tag (§13.2.2.4).
    pub fn learn_response(&mut self, response: &SipResponse) {
        let status = response.status();
        let other_dialog =
            self.confirmed && response.to().tag() != Some(self.dialog.remote_tag.as_str());
        if establishes_dialog(response) && !other_dialog {
            if let Some(tag) = response.to().tag() {
                self.dialog.remote_tag = tag.to_string();
            }
            let routes: Vec<String> = response
                .raw_text(sip_message::HeaderName::RecordRoute)
                .map(|s| s.to_string())
                .collect();
            if !routes.is_empty() {
                self.dialog.route_set = routes.into_iter().rev().collect();
            }
        }
        if status < 300 && !other_dialog {
            if let Some(contact) = response.contacts().as_slice().first() {
                self.dialog.remote_target = contact.uri().to_string();
            }
        }
        let mut rseq = None;
        if (101..200).contains(&response.status()) && reliably(response) {
            if let Some(seen) = rseq_of(response) {
                rseq = Some(seen);
                self.last_rseq = rseq;
                self.unacknowledged.push(ReliableProvisional {
                    rseq: seen,
                    cseq: response.cseq().seq(),
                    cseq_method: response.cseq().method().clone(),
                    to_tag: response.to().tag().map(str::to_string),
                });
            }
        }
        // A provisional carrying a To-tag opens an early dialog on this side
        // (RFC 3261 §12.1.2): the fork is the REMOTE's tag, and its target and
        // route set are the ones this provisional carried. Its local sequence
        // is seeded by the INVITE that created it (RFC 3261 §12.2.1.1) — never
        // the leg's running counter, which another fork's traffic may have
        // advanced by the time this fork rings.
        let provisional_fork =
            (101..200).contains(&response.status()) && *response.cseq().method() == Method::Invite;
        if let (true, Some(tag)) = (provisional_fork, response.to().tag().map(str::to_string)) {
            let seeded = self.sent_invite().map(|invite| invite.cseq().seq());
            let fork = StackDialog {
                local_cseq: seeded.unwrap_or(self.dialog.local_cseq),
                ..self.dialog.clone()
            };
            self.ring_early(&tag, fork, rseq);
        }
        // A 2xx to a PRACK or UPDATE an early dialog carried confirms nothing
        // (RFC 3261 §12.1, RFC 3262 §4, RFC 3311 §5.1): the fork keeps its own
        // sequence, and the INVITE's 2xx continues it.
        let answers_early_request =
            !self.confirmed && matches!(response.cseq().method(), Method::Prack | Method::Update);
        if response.status() >= 200 && response.status() < 300 && !answers_early_request {
            self.has_dialog = true;
            if let Some(tag) = response.to().tag() {
                let fork = self.early_dialog(tag).map(|early| early.dialog.local_cseq);
                if !self.confirmed && *response.cseq().method() == Method::Invite {
                    self.confirmed = true;
                    // The fork that answered is the dialog that confirmed, and
                    // the leg continues ITS sequence (RFC 3261 §12.2.1.1) —
                    // never a number another fork burned. A tag that rang no
                    // provisional confirms a dialog whose only prior request
                    // was the INVITE, so the INVITE's own CSeq seeds it.
                    let seeded = self.sent_invite().map(|invite| invite.cseq().seq());
                    self.dialog.local_cseq = fork.or(seeded).unwrap_or(self.dialog.local_cseq);
                } else {
                    // A non-confirming 2xx under a fork's tag syncs the leg to
                    // the numbers that fork has burned, so a later request the
                    // leg composes itself cannot reuse one of them.
                    self.dialog.local_cseq = self.dialog.local_cseq.max(fork.unwrap_or(0));
                }
            }
        }
    }

    /// Learn the dialog facts an inbound request carries, and keep it so a
    /// later response can answer it. The remote tag is learned only until a
    /// 2xx confirms the dialog, which fixes it (RFC 3261 §12.1.2); a request
    /// under another fork's tag then teaches the confirmed dialog nothing.
    pub fn learn_request(&mut self, request: &SipRequest) {
        let tag = request.from().tag().map(|t| t.to_string()).unwrap_or_default();
        if self.confirmed && !tag.is_empty() && tag != self.dialog.remote_tag {
            self.received.push(request.clone());
            return;
        }
        if !tag.is_empty() && !self.confirmed {
            self.dialog.remote_tag = tag;
        }
        // RFC 3261 §12.1.1 / §12.2.1.1 set the dialog's URIs once, from the
        // creating request; a request inside the dialog changes neither. The
        // display name and header parameters ride along so the scripted peer
        // writes its identity as the captured one did.
        if self.dialog.remote_uri.is_empty() {
            self.dialog.remote_uri = request.from().clone().without_tag().to_string();
        }
        if let Some(contact) = request.contacts().as_slice().first() {
            self.dialog.remote_target = contact.uri().to_string();
        }
        let routes: Vec<String> =
            request.raw_text(sip_message::HeaderName::RecordRoute).map(|s| s.to_string()).collect();
        if !routes.is_empty() {
            self.dialog.route_set = routes;
        }
        if request.method() == Method::Invite {
            self.dialog.call_id = request.call_id().to_string();
            if self.dialog.local_uri.is_empty() {
                self.dialog.local_uri = request.to().clone().without_tag().to_string();
            }
        }
        self.received.push(request.clone());
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

/// The URI a lane names as text. A value the reader accepts IS the URI; one it
/// does not is carried whole, so the peer sees what the lane wrote.
fn uri_of(text: &str) -> Uri {
    Uri::parse_or_verbatim(&SipStr::owned(text))
}

fn addr_of(text: &str) -> NameAddr {
    NameAddr::parse(&SipStr::owned(text)).unwrap_or_else(|_| NameAddr::new(uri_of(text)))
}

fn media_type(text: &str) -> MediaType {
    use sip_message::header::HeaderValue;
    MediaType::parse(&SipStr::owned(text)).unwrap_or_else(|_| MediaType::new(SipStr::owned(text)))
}

/// Whether a provisional is reliable (RFC 3262 §3: `Require: 100rel`). Header
/// reading is `sip-message`'s.
pub(crate) fn reliably(response: &SipResponse) -> bool {
    response
        .header::<header::Require>()
        .and_then(Result::ok)
        .is_some_and(|tokens| tokens.contains("100rel"))
}

/// The `RSeq` a response carries, where it carries a readable one.
pub(crate) fn rseq_of(response: &SipResponse) -> Option<u32> {
    response.header::<header::RSeq>().and_then(Result::ok).map(|r| r.value())
}

/// The document's frozen headers, as generator input.
fn frozen(headers: &[TemplateHeader]) -> Vec<SipHeader> {
    headers.iter().map(|h| SipHeader::new(h.name.clone(), h.value.clone())).collect()
}

/// Whether a response creates or confirms a dialog on the side that sent the
/// request: a 1xx (other than 100) or 2xx to an INVITE or SUBSCRIBE (RFC 3261
/// §12.1, RFC 6665 §4.1.2). Every other response states no dialog fact.
fn establishes_dialog(response: &SipResponse) -> bool {
    let status = response.status();
    let creating = matches!(response.cseq().method(), Method::Invite | Method::Subscribe);
    creating && (101..300).contains(&status)
}

fn out_of_dialog_method(method: &Method) -> Option<OutOfDialogMethod> {
    Some(match method {
        Method::Invite => OutOfDialogMethod::Invite,
        Method::Options => OutOfDialogMethod::Options,
        Method::Message => OutOfDialogMethod::Message,
        Method::Subscribe => OutOfDialogMethod::Subscribe,
        Method::Register => OutOfDialogMethod::Register,
        _ => return None,
    })
}

fn in_dialog_method(method: &Method) -> Option<InDialogMethod> {
    Some(match method {
        Method::Invite => InDialogMethod::Invite,
        Method::Update => InDialogMethod::Update,
        Method::Bye => InDialogMethod::Bye,
        Method::Info => InDialogMethod::Info,
        Method::Message => InDialogMethod::Message,
        Method::Prack => InDialogMethod::Prack,
        Method::Notify => InDialogMethod::Notify,
        Method::Options => InDialogMethod::Options,
        Method::Refer => InDialogMethod::Refer,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
