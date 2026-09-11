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
        LegStack {
            dialog: StackDialog {
                call_id: format!("{leg}-{run_nonce}@pivot.invalid"),
                local_tag: format!("{leg}-{run_nonce}-tag"),
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
        }
    }

    pub fn has_dialog(&self) -> bool {
        self.has_dialog
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

    /// The Call-ID this leg carries.
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
        header::Contact::from_uri(
            Uri::sip_user(SipStr::owned(&self.leg), SipStr::owned(&self.addr.ip().to_string()))
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
    /// request.
    pub fn in_dialog(
        &mut self,
        method: Method,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        if !self.has_dialog {
            return Err(StackError::NoDialog { leg: self.leg.clone(), method: method.to_string() });
        }
        let verb = in_dialog_method(&method).ok_or_else(|| StackError::UnsupportedMethod {
            leg: self.leg.clone(),
            method: method.to_string(),
        })?;
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
        let result = generate_in_dialog_request(verb, &self.dialog, &opts);
        self.dialog = result.dialog;
        if method == Method::Invite {
            self.sent_invites.push(result.request.clone());
        }
        Ok(result.request)
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
        let Answer { status, reason, cseq_method, early_tag } = *answer;
        let wanted = cseq_method.map(Method::from_wire);
        let matching = || {
            self.received
                .iter()
                .rev()
                .filter(|r| wanted.as_ref().is_none_or(|m| r.cseq().method() == m))
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
    /// document that states an unrelated number on the second — two forks of a
    /// capture the cut folded into one — is renumbered onto the space it rides.
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
        if self.has_dialog {
            return self.in_dialog(Method::Update, headers, body, content_type, cseq);
        }
        let tag = match early {
            Some(named) => named.to_string(),
            None => match self.early.len() {
                0 => {
                    return Err(StackError::NoDialog {
                        leg: self.leg.clone(),
                        method: Method::Update.to_string(),
                    })
                }
                1 => self.early[0].0.clone(),
                dialogs => {
                    return Err(StackError::EarlyDialogAmbiguous {
                        leg: self.leg.clone(),
                        method: Method::Update.to_string(),
                        dialogs,
                    })
                }
            },
        };
        let dialog =
            self.early_dialog(&tag).map(|early| early.dialog.clone()).ok_or_else(|| {
                StackError::EarlyDialogUnknown {
                    leg: self.leg.clone(),
                    method: Method::Update.to_string(),
                    early: tag.clone(),
                }
            })?;
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
        let result = generate_in_dialog_request(InDialogMethod::Update, &dialog, &opts);
        if let Some((_, early)) = self.early.iter_mut().find(|(known, _)| *known == tag) {
            early.dialog = result.dialog;
        }
        Ok(result.request)
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
        // A response under a number this leg never sent is a lane fault, not a
        // dialog fact; the newest INVITE keeps its ACK composable rather than
        // failing the step over it.
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
            Ok(generate_ack_for_2xx(Some(&txn), &self.dialog, &opts))
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

    /// Learn the dialog facts a response carries: the remote tag, the remote
    /// target and the route set (reversed Record-Route, RFC 3261 §12.1.2).
    pub fn learn_response(&mut self, response: &SipResponse) {
        let tag = response.to().tag().map(|t| t.to_string()).unwrap_or_default();
        if !tag.is_empty() {
            self.dialog.remote_tag = tag;
        }
        if let Some(contact) = response.contacts().as_slice().first() {
            self.dialog.remote_target = contact.uri().to_string();
        }
        let routes: Vec<String> = response
            .raw_text(sip_message::HeaderName::RecordRoute)
            .map(|s| s.to_string())
            .collect();
        if !routes.is_empty() {
            self.dialog.route_set = routes.into_iter().rev().collect();
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
        if response.status() >= 200 && response.status() < 300 {
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
    /// later response can answer it.
    pub fn learn_request(&mut self, request: &SipRequest) {
        let tag = request.from().tag().map(|t| t.to_string()).unwrap_or_default();
        if !tag.is_empty() {
            self.dialog.remote_tag = tag;
        }
        if self.dialog.remote_uri.is_empty() {
            self.dialog.remote_uri = request.from().uri().to_string();
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
            self.dialog.local_uri = request.to().uri().to_string();
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
fn reliably(response: &SipResponse) -> bool {
    response
        .header::<header::Require>()
        .and_then(Result::ok)
        .is_some_and(|tokens| tokens.contains("100rel"))
}

/// The `RSeq` a response carries, where it carries a readable one.
fn rseq_of(response: &SipResponse) -> Option<u32> {
    response.header::<header::RSeq>().and_then(Result::ok).map(|r| r.value())
}

/// The document's frozen headers, as generator input.
fn frozen(headers: &[TemplateHeader]) -> Vec<SipHeader> {
    headers.iter().map(|h| SipHeader::new(h.name.clone(), h.value.clone())).collect()
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
mod tests {
    use super::*;

    fn stack(leg: &str, nonce: &str) -> LegStack {
        let addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        LegStack::new(leg, addr, addr, nonce, "", "")
    }

    /// A UAS stack that has taken an inbound INVITE, and the INVITE itself.
    fn ringing(leg: &str) -> (LegStack, SipRequest) {
        let mut caller = stack("A", "r1a2b3");
        let addresses = Addresses {
            ruri: "sip:callee@pivot.invalid",
            from: "<sip:caller@pivot.invalid>",
            to: "<sip:callee@pivot.invalid>",
        };
        let invite = caller
            .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
            .expect("the caller composes its INVITE");
        let mut uas = stack(leg, "r1a2b3");
        uas.learn_request(&invite);
        (uas, invite)
    }

    fn reliable(rseq: &str) -> Vec<TemplateHeader> {
        vec![TemplateHeader::frozen("Require", "100rel"), TemplateHeader::frozen("RSeq", rseq)]
    }

    /// A caller stack that has put its INVITE on the wire.
    fn calling() -> LegStack {
        calling_with_cseq(None)
    }

    fn calling_with_cseq(cseq: Option<u32>) -> LegStack {
        let mut uac = stack("A", "r1a2b3");
        let addresses = Addresses {
            ruri: "sip:callee@pivot.invalid",
            from: "<sip:caller@pivot.invalid>",
            to: "<sip:callee@pivot.invalid>",
        };
        uac.out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, cseq)
            .expect("the caller composes its INVITE");
        uac
    }

    fn to_tag(response: &SipResponse) -> String {
        response.to().tag().expect("a response under a dialog carries a To-tag").to_string()
    }

    /// §6.1: each fork answers under its OWN early dialog, and two forks sharing
    /// a To-tag would be one dialog to every peer.
    #[test]
    fn two_forks_of_one_leg_answer_under_the_tags_the_run_minted() {
        let (mut uas, _) = ringing("B");
        let first = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("1"),
                Vec::new(),
                None,
            )
            .expect("fork 1 rings");
        let second = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f2"),
                },
                &reliable("7001"),
                Vec::new(),
                None,
            )
            .expect("fork 2 rings");
        assert_eq!(to_tag(&first), "B-early-f1");
        assert_eq!(to_tag(&second), "B-early-f2");
    }

    /// RFC 3261 §12.1.1: a request that already names its dialog is answered
    /// under THAT tag — a PRACK for one fork is never answered under another.
    #[test]
    fn a_response_repeats_the_to_tag_the_request_carried() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let rings = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("1"),
                Vec::new(),
                None,
            )
            .expect("fork 1 rings");
        uac.learn_response(&rings);
        let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs fork 1");
        assert_eq!(prack.to().tag(), Some("B-early-f1"), "a PRACK rides its own early dialog");
        uas.learn_request(&prack);
        let ok = uas
            .respond(
                &Answer {
                    status: 200,
                    reason: "OK",
                    cseq_method: Some("PRACK"),
                    early_tag: Some("B-early-f2"),
                },
                &[],
                Vec::new(),
                None,
            )
            .expect("the PRACK is answered");
        assert_eq!(to_tag(&ok), "B-early-f1", "the answered request names its own dialog");
    }

    /// The fork that ANSWERS is the dialog that confirmed, so the leg adopts its
    /// tag: everything in-dialog after it rides the dialog that was answered.
    #[test]
    fn the_fork_that_answers_becomes_the_leg_s_dialog() {
        let (mut uas, _) = ringing("B");
        uas.respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
        assert_ne!(uas.local_tag(), "B-early-f1", "ringing confirms nothing");
        uas.respond(
            &Answer {
                status: 200,
                reason: "OK",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("fork 1 answers");
        assert_eq!(uas.local_tag(), "B-early-f1");
    }

    /// RFC 3262 §3: the interpreter never invents the number a peer's RAck
    /// quotes.
    #[test]
    fn a_reliable_provisional_without_an_rseq_is_refused_by_name() {
        let (mut uas, _) = ringing("B");
        let refused = uas.respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: None,
            },
            &[TemplateHeader::frozen("Require", "100rel")],
            Vec::new(),
            None,
        );
        assert_eq!(refused, Err(StackError::NoRSeq { leg: "B".into(), status: 183 }));
    }

    /// RFC 3262 §7.2: the RAck is the provisional's own RSeq and the CSeq of the
    /// request it answers, read off the message that arrived.
    #[test]
    fn a_prack_acknowledges_the_provisional_the_leg_received() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let ringing_at_uas = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("7001"),
                Vec::new(),
                None,
            )
            .expect("the callee rings reliably");
        uac.learn_response(&ringing_at_uas);
        assert_eq!(uac.unacknowledged().len(), 1);
        let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs");
        assert_eq!(prack.method(), Method::Prack);
        assert_eq!(
            prack.raw_text(sip_message::HeaderName::RAck).next().map(|s| s.to_string()),
            Some("7001 1 INVITE".to_string())
        );
        assert!(
            uac.unacknowledged().is_empty(),
            "an acknowledged provisional is not PRACKed twice"
        );
    }

    /// A document that froze its own RAck replays it: a frozen header always
    /// wins over what the stack would compose.
    #[test]
    fn a_frozen_rack_wins_over_the_composed_one() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let rings = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("7001"),
                Vec::new(),
                None,
            )
            .expect("the callee rings reliably");
        uac.learn_response(&rings);
        let prack = uac
            .prack(None, &[TemplateHeader::frozen("RAck", "9 9 INVITE")], Vec::new(), None, None)
            .expect("the caller PRACKs");
        let racks: Vec<String> =
            prack.raw_text(sip_message::HeaderName::RAck).map(|s| s.to_string()).collect();
        assert_eq!(racks, ["9 9 INVITE"]);
    }

    #[test]
    fn a_prack_with_nothing_to_acknowledge_is_refused_by_name() {
        let mut uac = calling();
        assert_eq!(
            uac.prack(None, &[], Vec::new(), None, None),
            Err(StackError::NoReliableProvisional { leg: "A".into() })
        );
    }

    /// A caller whose callee rang two reliable forks: `f1` under RSeq 1, then
    /// `f2` under RSeq 7001, both outstanding.
    fn called_by_two_forks() -> LegStack {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        for (status, reason, tag, rseq) in
            [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
        {
            let rings = uas
                .respond(
                    &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                    &reliable(rseq),
                    Vec::new(),
                    None,
                )
                .expect("a fork rings");
            uac.learn_response(&rings);
        }
        uac
    }

    /// RFC 3261 §12.2.1.1: each early dialog one INVITE creates carries its own
    /// local sequence, seeded by that INVITE — so the next request in EACH fork
    /// is CSeq 2, and two forks legitimately carry the same number.
    #[test]
    fn two_forks_of_one_leg_number_their_own_pracks_from_the_invite_s_cseq() {
        let mut uac = called_by_two_forks();
        let first =
            uac.prack(Some("B-early-f1"), &[], Vec::new(), None, None).expect("fork 1's PRACK");
        let second =
            uac.prack(Some("B-early-f2"), &[], Vec::new(), None, None).expect("fork 2's PRACK");
        assert_eq!(first.to().tag(), Some("B-early-f1"));
        assert_eq!(second.to().tag(), Some("B-early-f2"));
        assert_eq!(first.cseq().seq(), 2, "fork 1's INVITE was CSeq 1");
        assert_eq!(second.cseq().seq(), 2, "fork 2's own space, not fork 1's leavings");
    }

    /// RFC 3262 §7.2 under interleaving: a PRACK acknowledges the oldest
    /// un-acknowledged provisional OF ITS FORK, so the To-tag and the RAck
    /// name one dialog whatever order the forks rang in.
    #[test]
    fn a_prack_acknowledges_the_oldest_provisional_of_its_own_fork() {
        let mut uac = called_by_two_forks();
        let rack = |request: &SipRequest| {
            request.raw_text(sip_message::HeaderName::RAck).next().map(|s| s.to_string())
        };
        let late = uac
            .prack(Some("B-early-f2"), &[], Vec::new(), None, None)
            .expect("fork 2 is PRACKed first");
        assert_eq!(late.to().tag(), Some("B-early-f2"));
        assert_eq!(rack(&late), Some("7001 1 INVITE".into()), "fork 2's own RSeq");
        let early = uac
            .prack(Some("B-early-f1"), &[], Vec::new(), None, None)
            .expect("fork 1 is PRACKed across the interleave");
        assert_eq!(early.to().tag(), Some("B-early-f1"));
        assert_eq!(rack(&early), Some("1 1 INVITE".into()), "fork 1's own RSeq");
        assert!(uac.unacknowledged().is_empty(), "each fork's provisional is acknowledged once");
    }

    /// A step naming no fork on a leg ringing ONE keeps the leg's ladder: the
    /// oldest provisional overall, the fork's tag, and consecutive CSeqs.
    #[test]
    fn a_prack_naming_no_fork_on_a_leg_ringing_one_keeps_the_leg_s_ladder() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let prack_next = |uac: &mut LegStack, uas: &mut LegStack, rseq: &str| {
            let rings = uas
                .respond(
                    &Answer {
                        status: 180,
                        reason: "Ringing",
                        cseq_method: Some("INVITE"),
                        early_tag: Some("B-early-f1"),
                    },
                    &reliable(rseq),
                    Vec::new(),
                    None,
                )
                .expect("the fork rings");
            uac.learn_response(&rings);
            uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs")
        };
        let first = prack_next(&mut uac, &mut uas, "7001");
        assert_eq!(first.cseq().seq(), 2);
        assert_eq!(first.to().tag(), Some("B-early-f1"));
        let second = prack_next(&mut uac, &mut uas, "7002");
        assert_eq!(second.cseq().seq(), 3, "one fork is one ladder");
        assert_eq!(second.to().tag(), Some("B-early-f1"));
    }

    /// A fork that rings AFTER another fork's PRACK exchange still seeds its
    /// local sequence from the INVITE (RFC 3261 §12.2.1.1): the 200 to fork
    /// 1's PRACK advances nothing in fork 2's space, so fork 2's PRACK is
    /// CSeq 2, not 3.
    #[test]
    fn a_fork_ringing_after_another_s_prack_still_seeds_from_the_invite() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let rings = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("70789988"),
                Vec::new(),
                None,
            )
            .expect("fork 1 rings");
        uac.learn_response(&rings);
        let first = uac.prack(None, &[], Vec::new(), None, None).expect("fork 1 is PRACKed");
        assert_eq!(first.cseq().seq(), 2);
        uas.learn_request(&first);
        let prack_ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("PRACK"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("fork 1's PRACK is answered");
        uac.learn_response(&prack_ok);
        let rings_again = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f2"),
                },
                &reliable("43886616"),
                Vec::new(),
                None,
            )
            .expect("fork 2 rings after the exchange");
        uac.learn_response(&rings_again);
        let second = uac.prack(None, &[], Vec::new(), None, None).expect("fork 2 is PRACKed");
        assert_eq!(second.to().tag(), Some("B-early-f2"));
        assert_eq!(second.cseq().seq(), 2, "fork 2's INVITE was its CSeq 1");
    }

    #[test]
    fn a_prack_naming_a_fork_the_leg_never_rang_is_refused_by_name() {
        let mut uac = called_by_two_forks();
        assert_eq!(
            uac.prack(Some("B-early-f9"), &[], Vec::new(), None, None),
            Err(StackError::EarlyDialogUnknown {
                leg: "A".into(),
                method: "PRACK".into(),
                early: "B-early-f9".into(),
            })
        );
    }

    /// `${leg:<id>.rseq}` reads the last reliable provisional the leg sighted,
    /// whichever side of the wire it was on.
    #[test]
    fn the_leg_publishes_the_rseq_it_last_sighted() {
        let (mut uas, _) = ringing("B");
        assert_eq!(uas.rseq(), None);
        uas.respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
        assert_eq!(uas.rseq(), Some(1));
        uas.respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("fork 2 rings");
        assert_eq!(uas.rseq(), Some(7001));
    }

    /// K7's answer: RFC 3262 §3 numbers an RSeq space PER early dialog, so each
    /// fork publishes its own — which is what `${early:<id>.rseq}` reads and
    /// what the leg's single value cannot say.
    #[test]
    fn each_fork_publishes_the_rseq_of_its_own_reliable_provisional() {
        let (mut uas, _) = ringing("B");
        uas.respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
        uas.respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("fork 2 rings");
        assert_eq!(uas.early_rseq("B-early-f1"), Some(1));
        assert_eq!(uas.early_rseq("B-early-f2"), Some(7001));
        assert_eq!(uas.early_rseq("B-early-f9"), None);
        assert_eq!(uas.early_tags().collect::<Vec<_>>(), ["B-early-f1", "B-early-f2"]);
    }

    /// RFC 3262 §3: a second reliable provisional on ONE early dialog is
    /// greater by exactly one, whatever unrelated number the document states —
    /// a capture whose two forks the cut folded into one fork numbers here.
    #[test]
    fn a_second_reliable_provisional_on_one_fork_rises_by_exactly_one() {
        let (mut uas, _) = ringing("B");
        let first = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("1318758475"),
                Vec::new(),
                None,
            )
            .expect("the fork rings");
        let second = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("705007309"),
                Vec::new(),
                None,
            )
            .expect("the fork rings again");
        assert_eq!(rseq_of(&first), Some(1318758475), "the document seeds the space");
        assert_eq!(rseq_of(&second), Some(1318758476));
        assert_eq!(uas.early_rseq("B-early-f1"), Some(1318758476));
        assert_eq!(uas.rseq(), Some(1318758476));
    }

    /// The same number twice is the SAME provisional retransmitted (RFC 3262
    /// §3), so it keeps it and the space does not advance.
    #[test]
    fn a_reliable_provisional_the_document_repeats_keeps_its_number() {
        let (mut uas, _) = ringing("B");
        for _ in 0..2 {
            let rings = uas
                .respond(
                    &Answer {
                        status: 180,
                        reason: "Ringing",
                        cseq_method: Some("INVITE"),
                        early_tag: Some("B-early-f1"),
                    },
                    &reliable("7001"),
                    Vec::new(),
                    None,
                )
                .expect("the fork rings");
            assert_eq!(rseq_of(&rings), Some(7001));
        }
        assert_eq!(uas.early_rseq("B-early-f1"), Some(7001));
    }

    /// The caller's side of the same fact: a provisional carrying a To-tag opens
    /// an early dialog (RFC 3261 §12.1.2), and each fork's is its own.
    #[test]
    fn a_caller_opens_one_early_dialog_per_fork_that_rings_it() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        for (status, reason, tag, rseq) in
            [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
        {
            let rings = uas
                .respond(
                    &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                    &reliable(rseq),
                    Vec::new(),
                    None,
                )
                .expect("a fork rings");
            uac.learn_response(&rings);
        }
        assert_eq!(uac.early_tags().collect::<Vec<_>>(), ["B-early-f1", "B-early-f2"]);
        assert_eq!(uac.early_rseq("B-early-f1"), Some(1));
        assert_eq!(uac.early_rseq("B-early-f2"), Some(7001));
    }

    // ── the in-dialog drive (§6.1) ──────────────────────────────────────────

    /// An established leg: the UAS took the INVITE and answered it 200, so
    /// everything it exchanges from here is in-dialog.
    fn established(leg: &str) -> LegStack {
        let (mut uas, _) = ringing(leg);
        uas.respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
        uas
    }

    fn header(request: &SipRequest, name: sip_message::HeaderName) -> String {
        request.raw_text(name).next().map(|s| s.to_string()).unwrap_or_default()
    }

    /// §6.1: an in-dialog request rides the dialog the leg ALREADY owns. A
    /// re-INVITE, an UPDATE and an INFO are one dialog's traffic, not three, so
    /// none of them mints a second Call-ID or a second pair of tags.
    #[test]
    fn every_in_dialog_request_rides_the_dialog_the_leg_already_owns() {
        let mut uas = established("B");
        let call_id = uas.call_id().to_string();
        let local_tag = uas.local_tag().to_string();
        let remote_tag = uas.remote_tag().expect("the INVITE carried a From-tag").to_string();
        for method in [Method::Invite, Method::Update, Method::Info] {
            let request = uas
                .in_dialog(method.clone(), &[], Vec::new(), None, None)
                .unwrap_or_else(|e| panic!("{method} composes in-dialog: {e}"));
            assert_eq!(request.call_id().to_string(), call_id, "{method} keeps the Call-ID");
            assert_eq!(request.from().tag(), Some(local_tag.as_str()), "{method} From-tag");
            assert_eq!(request.to().tag(), Some(remote_tag.as_str()), "{method} To-tag");
        }
    }

    /// RFC 3261 §12.2.1.1: each side numbers its OWN CSeq space. A leg that has
    /// taken requests at the peer's numbering still counts from its own.
    #[test]
    fn a_leg_continues_its_own_cseq_space_whatever_the_peer_numbers() {
        let mut uas = established("B");
        assert_eq!(uas.remote_cseq(), Some(1), "the peer's INVITE was its CSeq 1");
        let first = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("an INFO");
        assert_eq!(first.cseq().seq(), 1, "this side's first request is its own CSeq 1");
        let second =
            uas.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        assert_eq!(second.cseq().seq(), 2);
        assert_eq!(uas.local_cseq(), Some(2));
        // And a request the PEER sends next does not move this side's counter.
        // The peer's own INFO is composed at ITS numbering, which the document
        // never states and the two stacks never share.
        let mut peer = established("A-peer");
        peer.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("the peer's first INFO");
        let inbound = peer
            .in_dialog(Method::Info, &[], Vec::new(), None, None)
            .expect("the peer's second INFO");
        assert_eq!(inbound.cseq().seq(), 2, "the peer counts in its own space");
        uas.learn_request(&inbound);
        assert_eq!(uas.remote_cseq(), Some(2));
        let third = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("another INFO");
        assert_eq!(third.cseq().seq(), 3, "the peer's numbering is not this side's");
    }

    /// A `cseq-override` (§11) BECOMES the leg's sequence number: RFC 3261
    /// §12.2.1.1 numbers every later request from the dialog's own counter, so
    /// a jump the document states carries forward instead of being undone by
    /// the next request.
    #[test]
    fn an_overridden_cseq_is_emitted_and_becomes_the_leg_s_sequence_number() {
        let mut uas = established("B");
        let jumped = uas
            .in_dialog(Method::Bye, &[], Vec::new(), None, Some(5))
            .expect("the dialog is alive");
        assert_eq!(jumped.cseq().seq(), 5, "the emitted request carries the override");
        assert_eq!(uas.local_cseq(), Some(5));
        let next = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("an INFO");
        assert_eq!(next.cseq().seq(), 6, "the leg continues from the number it emitted");
    }

    /// The same rule on the dialog-OPENING request: the override numbers the
    /// INVITE, and the transaction it opens keeps that number.
    #[test]
    fn an_overridden_cseq_numbers_a_dialog_opening_invite_and_its_own_ack() {
        let mut uac = calling_with_cseq(Some(9));
        let invite = uac.sent_invite().expect("the caller dialled").clone();
        assert_eq!(invite.cseq().seq(), 9);
        let (mut uas, _) = ringing("B");
        uas.learn_request(&invite);
        let ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers");
        uac.learn_response(&ok);
        let ack = uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
        assert_eq!(ack.cseq().seq(), 9, "the ACK repeats the INVITE's number");
    }

    /// RFC 3261 §13.2.1: the ACK to a 2xx carries the CSeq NUMBER of the INVITE
    /// it answers — the re-INVITE's, not the dialog-opening one's.
    #[test]
    fn the_ack_to_a_re_invite_s_2xx_carries_the_re_invite_s_cseq() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers");
        uac.learn_response(&ok);
        let first_ack = uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
        assert_eq!(first_ack.cseq().seq(), 1);

        let re_invite =
            uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        assert_eq!(re_invite.cseq().seq(), 2);
        uas.learn_request(&re_invite);
        let re_ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers the re-INVITE");
        uac.learn_response(&re_ok);
        let ack = uac
            .ack_for(&re_ok, &[], Vec::new(), None, None)
            .expect("the caller ACKs the re-INVITE");
        assert_eq!(ack.cseq().seq(), 2, "the ACK answers the transaction that was answered");
        assert_eq!(ack.method(), Method::Ack);
    }

    /// A `cseq-override` (§11) on the ACK to a 2xx puts the captured number on
    /// the wire while the ACK still acknowledges the INVITE the response
    /// answered: the transaction is found by the RESPONSE's CSeq, so the leg
    /// owes no ACK afterwards and its own counter has not moved.
    #[test]
    fn an_overridden_cseq_reaches_the_ack_without_moving_the_leg_s_own_state() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers");
        uac.learn_response(&ok);
        uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");

        let re_invite =
            uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        assert_eq!(re_invite.cseq().seq(), 2);
        uas.learn_request(&re_invite);
        let re_ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers the re-INVITE");
        uac.learn_response(&re_ok);
        // The captured caller ACKs the re-INVITE's 2xx under the PREVIOUS
        // number, which is the defect the document states.
        let stale = uac
            .ack_for(&re_ok, &[], Vec::new(), None, Some(1))
            .expect("the caller ACKs the re-INVITE");
        assert_eq!(stale.cseq().seq(), 1, "the wire carries the overridden number");
        assert_eq!(stale.method(), Method::Ack);
        assert_eq!(
            uac.outstanding_invites().collect::<Vec<_>>(),
            Vec::<u32>::new(),
            "the ACK still acknowledges the INVITE the 2xx answered"
        );
        assert_eq!(uac.local_cseq(), Some(2), "an ACK numbers nothing: the counter stands");
        let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("the dialog ends");
        assert_eq!(bye.cseq().seq(), 3, "the leg continues from the re-INVITE, not the override");
    }

    /// RFC 3261 §17.1.1.3: the ACK to a non-2xx final is composed by the INVITE
    /// client transaction, so a `cseq-override` on it is refused by name rather
    /// than dropped — an emission that quietly kept the compliant number would
    /// run green without reproducing the defect.
    #[test]
    fn a_cseq_override_on_the_ack_to_a_negative_final_is_refused_by_name() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let refused = uas
            .respond(
                &Answer {
                    status: 486,
                    reason: "Busy Here",
                    cseq_method: Some("INVITE"),
                    early_tag: None,
                },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee refuses");
        assert_eq!(
            uac.ack_for(&refused, &[], Vec::new(), None, Some(7)),
            Err(StackError::CseqOverrideOnAbsorbedAck { leg: "A".into() })
        );
    }

    /// RFC 3261 §13.2.1: where the INVITE carried no offer, the ANSWER rides
    /// the ACK — so the step's stored body and its frozen headers go out on it.
    #[test]
    fn the_ack_to_a_2xx_carries_the_step_s_body_and_frozen_headers() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers");
        uac.learn_response(&ok);
        let answer = b"v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\n";
        let ack = uac
            .ack_for(
                &ok,
                &[TemplateHeader::frozen("P-Charging-Vector", "icid-value=abc")],
                answer.to_vec(),
                Some("application/sdp".into()),
                None,
            )
            .expect("the caller ACKs");
        assert_eq!(ack.body(), answer.as_slice(), "the delayed-offer answer rides the ACK");
        assert_eq!(
            ack.raw_text(sip_message::HeaderName::ContentType).next().map(|v| v.to_string()),
            Some("application/sdp".to_string())
        );
        assert_eq!(
            ack.raw_text(sip_message::HeaderName::Other("P-Charging-Vector".into()))
                .next()
                .map(|v| v.to_string()),
            Some("icid-value=abc".to_string())
        );
    }

    /// RFC 3261 §17.1.1.3: the ACK to a NON-2xx is part of the INVITE client
    /// transaction, so it reuses that INVITE's own top Via branch — the
    /// re-INVITE's, which is what the absorption seam keys the obligation on.
    #[test]
    fn the_ack_to_an_in_dialog_negative_final_rides_the_re_invite_s_own_branch() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the callee answers");
        uac.learn_response(&ok);
        uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");

        let re_invite =
            uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        let branch = re_invite.top_via().branch().unwrap_or_default().to_string();
        uas.learn_request(&re_invite);
        let refused = uas
            .respond(
                &Answer {
                    status: 488,
                    reason: "Not Acceptable Here",
                    cseq_method: Some("INVITE"),
                    early_tag: None,
                },
                &[],
                Vec::new(),
                None,
            )
            .expect("the peer refuses the offer");
        let ack =
            uac.ack_for(&refused, &[], Vec::new(), None, None).expect("the caller ACKs the 488");
        assert_eq!(ack.top_via().branch().unwrap_or_default(), branch.as_str());
        assert_eq!(ack.cseq().seq(), re_invite.cseq().seq());
        // A negative in-dialog final closes nothing: the dialog stands and the
        // next request keeps numbering where the re-INVITE left off (K15).
        let bye =
            uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("the dialog is alive");
        assert_eq!(bye.cseq().seq(), 3);
    }

    /// An in-dialog request on a leg that has no dialog is REFUSED by name.
    /// Composing one would mint a second dialog identity for a leg that already
    /// has one coming, which is a wrong run reported as a green one.
    #[test]
    fn an_in_dialog_request_on_a_leg_with_no_dialog_is_refused_by_name() {
        let mut fresh = stack("B", "r1a2b3");
        assert_eq!(
            fresh.in_dialog(Method::Info, &[], Vec::new(), None, None),
            Err(StackError::NoDialog { leg: "B".into(), method: "INFO".into() })
        );
    }

    // ── UPDATE on an early dialog (RFC 3311 §5.1) ───────────────────────────

    /// A UAS ringing two forks, each under its own tag and RSeq.
    fn forking(leg: &str) -> (LegStack, SipRequest) {
        let (mut uas, invite) = ringing(leg);
        uas.respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
        uas.respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("fork 2 rings");
        (uas, invite)
    }

    const OFFER: &[u8] = b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
        t=0 0\r\nm=audio 20001 RTP/AVP 0\r\n";

    /// RFC 3311 §5.1: an UPDATE runs INSIDE an early dialog. The fork the step
    /// names is the dialog it rides — its tag on the From, the peer's on the
    /// To — and the offer it carries goes with it.
    #[test]
    fn an_update_rides_the_early_dialog_the_step_names() {
        let (mut uas, invite) = forking("B");
        let caller_tag = invite.from().tag().expect("the INVITE carried a From-tag").to_string();
        let update = uas
            .update(Some("B-early-f2"), &[], OFFER.to_vec(), Some("application/sdp".into()), None)
            .expect("the second fork updates its own early dialog");
        assert_eq!(update.method(), Method::Update);
        assert_eq!(update.from().tag(), Some("B-early-f2"), "the UPDATE rides fork 2");
        assert_eq!(update.to().tag(), Some(caller_tag.as_str()), "the peer's tag is the caller's");
        assert_eq!(update.call_id().to_string(), uas.call_id(), "one leg, one Call-ID");
        assert_eq!(update.body().as_ref(), OFFER, "the offer rides the UPDATE");
        assert_eq!(header(&update, sip_message::HeaderName::ContentType), "application/sdp");
        // RFC 3261 §12.2.1.1: each dialog numbers its OWN local sequence, and
        // this side has sent nothing on this one before.
        assert_eq!(update.cseq().seq(), 1);
        let second = uas
            .update(Some("B-early-f2"), &[], Vec::new(), None, None)
            .expect("the fork updates again");
        assert_eq!(second.cseq().seq(), 2, "the fork continues its own numbering");
        // And the OTHER fork numbers from its own zero, not fork 2's.
        let other = uas
            .update(Some("B-early-f1"), &[], Vec::new(), None, None)
            .expect("the first fork updates its own early dialog");
        assert_eq!(other.from().tag(), Some("B-early-f1"));
        assert_eq!(other.cseq().seq(), 1);
    }

    /// The caller's direction of the same request: the fork is the REMOTE's tag,
    /// and the CSeq continues the caller's own numbering from its INVITE.
    #[test]
    fn a_caller_s_update_on_its_one_early_dialog_needs_no_name() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let rings = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("1"),
                Vec::new(),
                None,
            )
            .expect("the callee rings");
        uac.learn_response(&rings);
        let update = uac
            .update(None, &[], OFFER.to_vec(), Some("application/sdp".into()), None)
            .expect("one early dialog needs no naming");
        assert_eq!(update.method(), Method::Update);
        assert_eq!(update.to().tag(), Some("B-early-f1"), "the UPDATE reaches the fork that rang");
        assert_eq!(update.from().tag(), Some(uac.local_tag()));
        assert_eq!(update.cseq().seq(), 2, "the caller's INVITE was its CSeq 1");
    }

    /// §14: the tool infers nothing. A leg ringing two forks and an UPDATE
    /// naming neither is a refusal — which fork it rides decides who receives
    /// it — and so is a fork the leg never rang.
    #[test]
    fn an_update_that_cannot_tell_which_fork_it_rides_is_refused_by_name() {
        let (mut uas, _) = forking("B");
        assert_eq!(
            uas.update(None, &[], Vec::new(), None, None),
            Err(StackError::EarlyDialogAmbiguous {
                leg: "B".into(),
                method: "UPDATE".into(),
                dialogs: 2,
            })
        );
        assert_eq!(
            uas.update(Some("B-early-f9"), &[], Vec::new(), None, None),
            Err(StackError::EarlyDialogUnknown {
                leg: "B".into(),
                method: "UPDATE".into(),
                early: "B-early-f9".into(),
            })
        );
        // A leg that has rung NO fork and confirmed no dialog has no dialog at
        // all, which is the older refusal and stands.
        let mut fresh = stack("B", "r1a2b3");
        assert_eq!(
            fresh.update(None, &[], Vec::new(), None, None),
            Err(StackError::NoDialog { leg: "B".into(), method: "UPDATE".into() })
        );
    }

    /// Once the dialog confirms, an UPDATE is ordinary in-dialog traffic on the
    /// dialog the leg owns — the fork that answered — and numbers itself in that
    /// dialog's sequence space whether or not the step still names the fork.
    #[test]
    fn an_update_after_confirmation_rides_the_confirmed_dialog() {
        let (mut uas, invite) = forking("B");
        let early = uas
            .update(Some("B-early-f1"), &[], Vec::new(), None, None)
            .expect("fork 1 updates before answering");
        assert_eq!(early.cseq().seq(), 1);
        uas.respond(
            &Answer {
                status: 200,
                reason: "OK",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("fork 1 answers");
        let confirmed = uas.update(None, &[], Vec::new(), None, None).expect("the dialog is up");
        assert_eq!(
            confirmed.from().tag(),
            Some("B-early-f1"),
            "the fork that answered IS the dialog"
        );
        assert_eq!(confirmed.to().tag(), invite.from().tag());
        assert_eq!(
            confirmed.cseq().seq(),
            2,
            "the confirmed dialog continues the fork's numbering"
        );
        // And the plain in-dialog path composes the same request.
        let plain = uas.in_dialog(Method::Update, &[], Vec::new(), None, None).expect("in-dialog");
        assert_eq!(plain.cseq().seq(), 3);
    }

    /// RFC 3261 §12.2.1.1 at confirmation: the leg continues the CONFIRMED
    /// dialog's sequence. A number burned in ANOTHER fork does not carry across
    /// to the fork that answered.
    #[test]
    fn confirmation_adopts_the_answering_fork_s_own_sequence() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        for (status, reason, tag, rseq) in
            [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
        {
            let rings = uas
                .respond(
                    &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                    &reliable(rseq),
                    Vec::new(),
                    None,
                )
                .expect("a fork rings");
            uac.learn_response(&rings);
        }
        // Fork 1 burns its CSeq 2 on an UPDATE; fork 2's space stays untouched.
        let update = uac
            .update(Some("B-early-f1"), &[], Vec::new(), None, None)
            .expect("fork 1 updates its own early dialog");
        assert_eq!(update.cseq().seq(), 2);
        let ok = uas
            .respond(
                &Answer {
                    status: 200,
                    reason: "OK",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f2"),
                },
                &[],
                Vec::new(),
                None,
            )
            .expect("fork 2 answers");
        uac.learn_response(&ok);
        let re_invite =
            uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        assert_eq!(
            re_invite.cseq().seq(),
            2,
            "fork 2's only prior request was the INVITE's CSeq 1"
        );
    }

    /// A confirming To-tag that rang no provisional confirms a dialog whose
    /// only prior request was the INVITE, so the INVITE's own CSeq seeds the
    /// sequence the leg continues — never 0 and never the leg's running count.
    #[test]
    fn confirmation_under_a_tag_that_rang_nothing_seeds_from_the_invite() {
        let mut uac = calling();
        let (mut uas, _) = ringing("B");
        let rings = uas
            .respond(
                &Answer {
                    status: 183,
                    reason: "Session Progress",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("1"),
                Vec::new(),
                None,
            )
            .expect("fork 1 rings");
        uac.learn_response(&rings);
        // The one fork burns its CSeq 2 on an UPDATE, whose 200 syncs the leg.
        let update = uac.update(None, &[], Vec::new(), None, None).expect("the fork updates");
        assert_eq!(update.cseq().seq(), 2);
        uas.learn_request(&update);
        let update_ok = uas
            .respond(
                &Answer { status: 200, reason: "OK", cseq_method: Some("UPDATE"), early_tag: None },
                &[],
                Vec::new(),
                None,
            )
            .expect("the UPDATE is answered");
        uac.learn_response(&update_ok);
        // The INVITE is answered under a tag no provisional carried.
        let ok = uas
            .respond(
                &Answer {
                    status: 200,
                    reason: "OK",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f2"),
                },
                &[],
                Vec::new(),
                None,
            )
            .expect("the INVITE answers under a fresh tag");
        uac.learn_response(&ok);
        let re_invite =
            uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
        assert_eq!(re_invite.cseq().seq(), 2, "the confirmed dialog held only the INVITE's CSeq 1");
    }

    /// Every other in-dialog request still waits for a confirmed dialog: PRACK
    /// and UPDATE are the two that run inside an early one.
    #[test]
    fn an_in_dialog_request_that_is_no_update_still_waits_for_confirmation() {
        let (mut uas, _) = forking("B");
        assert_eq!(
            uas.in_dialog(Method::Info, &[], Vec::new(), None, None),
            Err(StackError::NoDialog { leg: "B".into(), method: "INFO".into() })
        );
        assert_eq!(
            uas.in_dialog(Method::Bye, &[], Vec::new(), None, None),
            Err(StackError::NoDialog { leg: "B".into(), method: "BYE".into() })
        );
    }

    /// A frozen body rides an in-dialog request with the media type the document
    /// states: an INFO's payload is the transfer intake's whole content.
    #[test]
    fn an_in_dialog_request_carries_the_body_and_media_type_the_step_states() {
        let mut uas = established("B");
        let info = uas
            .in_dialog(
                Method::Info,
                &[],
                b"dwdHAIQDjwGh".to_vec(),
                Some("application/example-binary".into()),
                None,
            )
            .expect("an INFO with a frozen body");
        assert_eq!(info.body().as_ref(), b"dwdHAIQDjwGh");
        assert_eq!(
            header(&info, sip_message::HeaderName::ContentType),
            "application/example-binary"
        );
        assert_eq!(header(&info, sip_message::HeaderName::ContentLength), "12");
    }

    /// Compile once, run MANY: two instances of one plan must not share a
    /// dialog identity, or the second run folds into the first's state.
    #[test]
    fn two_runs_of_one_leg_mint_disjoint_identities() {
        let first = stack("A", "r1a2b3");
        let second = stack("A", "r9z8y7");
        assert_ne!(first.call_id(), second.call_id());
        assert_ne!(first.local_tag(), second.local_tag());
        // The leg id stays in front, so a trace still reads.
        assert!(first.call_id().starts_with("A-"), "{}", first.call_id());
        assert!(first.local_tag().starts_with("A-"), "{}", first.local_tag());
    }

    #[test]
    fn two_legs_of_one_run_mint_disjoint_identities() {
        let a = stack("A", "r1a2b3");
        let b = stack("B", "r1a2b3");
        assert_ne!(a.call_id(), b.call_id());
        assert_ne!(a.local_tag(), b.local_tag());
    }

    /// §8.1.1.7: a branch is unique across TIME, not merely within a run.
    /// §17.2.3 matches a server transaction on branch + sent-by + method and
    /// never on the Call-ID, so two runs on one socket would otherwise merge.
    #[test]
    fn two_runs_mint_disjoint_via_branches() {
        let mut first = stack("A", "r1a2b3");
        let mut second = stack("A", "r9z8y7");
        let branches = |s: &mut LegStack| {
            (0..3).map(|_| s.via().branch().unwrap_or_default().to_string()).collect::<Vec<_>>()
        };
        let one = branches(&mut first);
        let two = branches(&mut second);
        assert!(one.iter().all(|b| b.starts_with("z9hG4bK")), "{one:?}");
        for branch in &one {
            assert!(!two.contains(branch), "{branch} minted by both runs");
        }
        // And each run's own branches stay distinct per transaction.
        let mut sorted = one.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), one.len(), "{one:?}");
    }
}
