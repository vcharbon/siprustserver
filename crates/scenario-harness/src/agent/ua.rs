//! [`Agent`] — the stateful fake UA: identity minting (branch/tag/Via/
//! Contact), the ONE fallible send/receive core (threaded through the §17.2
//! receive view), and the basic receive/dispatch primitives. The tolerant /
//! absorbing receive policies live in [`super::tolerant_recv`].

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_message::generators::{
    generate_in_dialog_request, generate_out_of_dialog_request, GenerateInDialogRequestOpts,
    GenerateOutOfDialogRequestOpts, InDialogMethod, OutOfDialogMethod, StackDialog,
};
use sip_message::header::{self, HeaderValue, NameAddr, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse, SipStr};
use sip_net::UdpEndpoint;

use super::client_txn::expect_response;
use super::dialog::InDialogTxn;
use super::harness::Ids;
use super::out_of_dialog::OutOfDialogRequest;
use super::rr_fold::RecordRouteFold;
use super::server_txn::ServerTxn;
use super::step::{unwrap_step, StepError};
use super::Invite;
use crate::absorption::{Absorption, Owner, TwoXxAcks, WireEntry};

/// The media type a scenario names as text. A value the reader rejects still
/// reaches the wire as the test wrote it — a scenario states the bytes it means
/// to send, including a deliberately awkward one.
pub(super) fn media_type(text: &str) -> header::MediaType {
    header::MediaType::parse(&SipStr::owned(text))
        .unwrap_or_else(|_| header::MediaType::new(SipStr::owned(text)))
}

/// The URI a scenario names as text. A test case states identities as strings
/// (they come from scenario data); the value model is what reaches the wire, and
/// an address no reader accepts is carried whole so the peer sees what the test
/// wrote.
pub(super) fn uri_of(text: &str) -> Uri {
    Uri::parse_or_verbatim(&SipStr::owned(text))
}

/// The address a scenario names, as a name-addr: a bare URI, a bracketed one
/// or a display-name form all read here, so a test states the identity in the
/// spelling its scenario uses and the wire carries it back.
fn addr_of(text: &str) -> NameAddr {
    NameAddr::parse(&SipStr::owned(text)).unwrap_or_else(|_| NameAddr::new(uri_of(text)))
}

/// The From a scenario names: the address plus this UA's local tag.
pub(super) fn from_of(uri: &str, tag: &str) -> header::From {
    header::From::new(addr_of(uri)).with_tag(SipStr::owned(tag))
}

/// The To a scenario names — tag-less, as an out-of-dialog request requires.
pub(super) fn to_of(uri: &str) -> header::To {
    header::To::new(addr_of(uri))
}

/// One inbound SIP message surfaced through the §17.2 receive view — a request
/// (as a UAS-side [`ServerTxn`]) or a response — WITHOUT asserting either the
/// kind or, for a request, the method. The reactive per-endpoint actor
/// ([`crate::actor`]) dispatches on this via [`Agent::recv_any`].
pub enum Inbound {
    /// A received request, wrapped in its UAS-side transaction.
    Request(ServerTxn),
    /// A received response (to one of our client transactions).
    Response(SipResponse),
}

/// A stateful fake UA. Cheap to clone (shares the endpoint + id source); the
/// dialog state lives on the per-transaction handles it returns, not here.
#[derive(Clone)]
pub struct Agent {
    // Fields are `pub(crate)` so the Send [`crate::loadbind::AgentBinder`] can
    // construct an `Agent` the same way `Harness::agent_with_roles` does,
    // without the `!Send` `Harness` wrapper. The fluent API is the public
    // surface.
    pub(crate) name: String,
    pub(crate) addr: SocketAddr,
    /// Dialog URI (`sip:name@ip`, no port) — used for From/To.
    pub(crate) uri: String,
    pub(crate) ep: Arc<dyn UdpEndpoint>,
    pub(crate) ids: Arc<Ids>,
    /// How this UA echoes multiple Record-Route rows when it acts as UAS
    /// ([`RecordRouteFold`]). Chosen per-UA at bind time.
    pub(crate) rr_fold: RecordRouteFold,
    /// Per-`recv` wait bound, inherited from the `Harness` (Endpoint config).
    pub(crate) recv_timeout: Duration,
    /// This UA's receive-side transaction layer ([`Absorption`]) — the §17.2
    /// classification, the §17.1.1.3 hop-ACK ledger, and the log both views
    /// project from. Shared across clones: one per logical UA.
    pub(crate) txn: Arc<Absorption>,
    /// The ACKs this UA's core sent to 2xx finals, so a retransmitted 2xx is
    /// answered again (§13.2.2.4). Shared across clones.
    pub(crate) two_xx_acks: Arc<TwoXxAcks>,
    /// Datagrams ALREADY sighted by [`sight_queued`](Agent::sight_queued) and
    /// kept by the TU view, waiting for the body's next pull. Every receive
    /// path drains it first, so sighting early never reorders the stream.
    pub(crate) holdback: Arc<Mutex<VecDeque<Result<SipMessage, StepError>>>>,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The identity of the UA STACK behind this handle: equal for clones of one
    /// agent, distinct for agents that merely share a bound address (the logical
    /// agents of a [`CalleeGroup`](crate::CalleeGroup) each keep their own). A
    /// consumer that must run ONE receive pump per stack groups actors on it.
    pub fn stack_id(&self) -> usize {
        Arc::as_ptr(&self.txn) as *const () as usize
    }

    /// Every datagram this UA received, in arrival order, each tagged with the
    /// view it belongs to (issue 22's table, in [`crate::absorption`]).
    pub fn wire_view(&self) -> Vec<WireEntry> {
        self.txn.wire_view()
    }

    /// What this UA's transaction user saw: the `SeenBy::Both` subset of
    /// [`wire_view`](Self::wire_view), in the same order.
    pub fn tu_view(&self) -> Vec<WireEntry> {
        self.txn.tu_view()
    }

    /// Stop absorbing duplicates: every repeat reaches this UA's pulls again.
    /// Reach for this only when a body must PULL each repeat itself — reading
    /// [`wire_view`](Self::wire_view) is how a test counts retransmissions now.
    /// Affects every clone of this UA.
    pub fn drop_to_raw_wire(&self) {
        self.txn.disable_dedup();
    }

    pub(super) fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    pub(super) fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    pub(super) fn via(&self) -> Via {
        Via::udp(SipStr::owned(&self.addr.ip().to_string()), self.addr.port())
            .with_branch(SipStr::owned(&self.branch()))
    }
    pub(super) fn contact(&self) -> header::Contact {
        header::Contact::from_uri(
            Uri::sip_user(SipStr::owned(&self.name), SipStr::owned(&self.addr.ip().to_string()))
                .with_port(self.addr.port()),
        )
    }

    /// Panicking veneer over [`try_send`](Agent::try_send).
    pub(super) async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        unwrap_step(self.try_send(msg, dst).await)
    }

    /// Panicking veneer over [`try_send_wire`](Agent::try_send_wire).
    pub(super) async fn send_wire(&self, wire: &[u8], dst: SocketAddr) {
        unwrap_step(self.try_send_wire(wire, dst).await)
    }

    /// Panicking veneer over [`try_recv`](Agent::try_recv).
    pub(super) async fn recv(&self) -> SipMessage {
        unwrap_step(self.try_recv().await)
    }

    /// THE send core: one datagram out, a transport error returned as
    /// [`StepError::Transport`]. The functional lane panics on it via
    /// [`send`](Agent::send); the best-effort teardown helpers
    /// ([`Dialog::bye_best_effort`](super::Dialog::bye_best_effort),
    /// [`CancelHandle`](super::CancelHandle)) the load driver runs on a failed
    /// call swallow it — a send must never abort the worker.
    pub(crate) async fn try_send(
        &self,
        msg: &SipMessage,
        dst: SocketAddr,
    ) -> Result<(), StepError> {
        // Every ACK this UA sends is cached (§13.2.2.4): if the final it
        // answers is retransmitted, the CORE owes the same ACK again. A hop ACK
        // to a non-2xx is cached too and never read — its final is absorbed as a
        // transaction-layer duplicate before the cache is ever consulted.
        if let SipMessage::Request(r) = msg {
            if r.method().as_str() == "ACK" {
                if let Some(tag) = r.to().tag() {
                    self.two_xx_acks.remember(
                        r.call_id().to_string(),
                        r.cseq().seq(),
                        tag.to_string(),
                        r.clone(),
                        dst,
                    );
                }
            }
        }
        self.try_send_wire(&serialize(msg), dst).await
    }

    /// **The datagram seam**, for a driver that owns its own dialog state and
    /// composes through `sip_message::generators` rather than through the
    /// fluent builders: one already-rendered datagram out, recorded exactly as
    /// [`try_send`](Agent::try_send) records it.
    ///
    /// Reach for it only when the caller IS the stack — a scenario interpreter
    /// sequencing arbitrary steps across legs cannot use the pull-shaped
    /// transaction handles, because it must dispatch what ARRIVES to whichever
    /// leg claims it. A test driving one call still uses the builders.
    pub async fn try_send_datagram(&self, wire: &[u8], dst: SocketAddr) -> Result<(), StepError> {
        self.try_send_wire(wire, dst).await
    }

    /// Send an ALREADY-rendered datagram. A message this UA froze carries its
    /// own image, and that image is its wire form — sending it needs no second
    /// render. The wire bytes are the recorder's input either way, so the trace
    /// is identical to the [`try_send`](Agent::try_send) path.
    pub(crate) async fn try_send_wire(
        &self,
        wire: &[u8],
        dst: SocketAddr,
    ) -> Result<(), StepError> {
        self.ep
            .send_to(wire, dst)
            .await
            .map_err(|e| StepError::Transport { who: self.name.clone(), detail: e.to_string() })
    }

    /// THE receive core: one SIP datagram surfaced through the §17.2 receive
    /// view ([`Absorption`]). A timeout / closed queue / parse error is a
    /// [`StepError`]; the functional lane panics on it via [`recv`](Agent::recv).
    ///
    /// Every surfaced ACK is also SIGHTED against the §17.1.1.3 obligation
    /// ledger ([`Absorption`]) — fulfilment is recorded here so it happens
    /// on ANY pull path, but the absorb decision stays with the caller
    /// ([`ack_obligation_claims`](Agent::ack_obligation_claims) at the
    /// would-be-error sites), so an explicit `receive("ACK")` keeps working.
    pub(super) async fn try_recv(&self) -> Result<SipMessage, StepError> {
        loop {
            if let Some(held) = self.take_held() {
                return held;
            }
            let pkt = match tokio::time::timeout(self.recv_timeout, self.ep.recv()).await {
                Err(_) => return Err(StepError::Timeout { who: self.name.clone() }),
                Ok(None) => return Err(StepError::QueueClosed { who: self.name.clone() }),
                Ok(Some(p)) => p,
            };
            let msg = match self.parse(&pkt.raw) {
                Ok(msg) => msg,
                Err(e) => return Err(e),
            };
            if let Some(msg) = self.surface(&pkt.raw, msg).await {
                return Ok(msg);
            }
        }
    }

    /// Bring the wire view current WITHOUT pulling: sight every datagram
    /// already queued on this UA's socket, so a retransmission the body will
    /// never pull is still in [`wire_view`](Agent::wire_view). What the TU view
    /// keeps is held back, in arrival order, for the body's next receive.
    pub async fn sight_queued(&self) {
        while let Some(pkt) = self.ep.try_recv() {
            let held = match self.parse(&pkt.raw) {
                Err(e) => Some(Err(e)),
                Ok(msg) => self.surface(&pkt.raw, msg).await.map(Ok),
            };
            if let Some(held) = held {
                self.holdback.lock().unwrap().push_back(held);
            }
        }
    }

    /// The next already-sighted datagram, if `sight_queued` left one.
    fn take_held(&self) -> Option<Result<SipMessage, StepError>> {
        self.holdback.lock().unwrap().pop_front()
    }

    /// The next TU-visible datagram available WITHOUT waiting: the holdback
    /// first, then whatever is queued on the socket, each sighted once.
    /// `None` when nothing is pending — the poll-advance idiom's stop signal.
    pub(crate) async fn take_held_or_queued(&self) -> Option<Result<SipMessage, StepError>> {
        loop {
            if let Some(held) = self.take_held() {
                return Some(held);
            }
            let pkt = self.ep.try_recv()?;
            match self.parse(&pkt.raw) {
                Err(e) => return Some(Err(e)),
                Ok(msg) => {
                    if let Some(msg) = self.surface(&pkt.raw, msg).await {
                        return Some(Ok(msg));
                    }
                }
            }
        }
    }

    fn parse(&self, raw: &[u8]) -> Result<SipMessage, StepError> {
        CustomParser::new()
            .parse(raw)
            .map_err(|e| StepError::Unparseable { who: self.name.clone(), detail: e.to_string() })
    }

    /// **The absorption seam**: classify one arriving datagram once
    /// ([`Absorption::sight`]) and decide whether this pull sees it.
    /// `None` means the transaction layer kept it — a §17.2 duplicate, or a
    /// retransmitted 2xx this UA's core answered by re-sending the ACK it owes
    /// (§13.2.2.4), which is the TU acting on a datagram the TU view keeps.
    /// A txn-owned hop ACK still SURFACES here: the pull sites claim it, so an
    /// explicit `receive("ACK")` keeps working.
    pub(crate) async fn surface(&self, raw: &[u8], msg: SipMessage) -> Option<SipMessage> {
        let sighting = self.txn.sight(raw, &msg);
        if sighting.owner == Owner::TxnDuplicate {
            return None;
        }
        if sighting.repeat {
            if let SipMessage::Response(r) = &msg {
                if let Some((ack, dst)) = self.two_xx_acks.owed_for(r) {
                    let _ = self.try_send(&SipMessage::Request(ack), dst).await;
                    return None;
                }
            }
        }
        Some(msg)
    }

    /// Whether `r` is the hop ACK of an armed §17.1.1.3 obligation on this UA.
    /// Marks the obligation fulfilled (idempotent). A receive path that would
    /// otherwise ERROR on an unexpected ACK calls this and absorbs instead —
    /// the ACK-races-the-next-INVITE interleave, in either order, never trips
    /// a body.
    pub(crate) fn ack_obligation_claims(&self, r: &SipRequest) -> bool {
        self.txn.hop_ack_claims(r)
    }

    /// THE request-receive core: receive the next request and check its method,
    /// returning a UAS-side transaction. A wrong method, an unexpected
    /// response, a timeout — all become a [`StepError`]; the functional lane
    /// panics on them via [`receive`](Agent::receive).
    ///
    /// A txn-owned hop ACK (an armed §17.1.1.3 obligation) that arrives ahead
    /// of the awaited request is absorbed, not an error — the
    /// ACK-before-the-next-INVITE interleave needs no body-side boilerplate.
    pub async fn try_receive(&self, method: &str) -> Result<ServerTxn, StepError> {
        loop {
            match self.try_recv().await? {
                SipMessage::Request(r) => {
                    if r.method() != method {
                        if self.ack_obligation_claims(&r) {
                            continue;
                        }
                        return Err(StepError::WrongMethod {
                            who: self.name.clone(),
                            expected: method.to_string(),
                            got: r.method().to_string(),
                        });
                    }
                    return Ok(ServerTxn::from_request(self.clone(), r));
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected a {method} request",
                            r.status(),
                            r.reason()
                        ),
                    })
                }
            }
        }
    }

    /// Receive the next inbound message of EITHER kind through the shared §17.2
    /// receive view ([`Absorption`]) — the reactive-actor primitive (the
    /// [`crate::actor`] reactor dispatches on this instead of asserting one
    /// expected message, so a late / reordered / retransmitted datagram is
    /// always consumed). A timeout / closed queue / parse error is a
    /// [`StepError`] exactly as [`try_receive`](Agent::try_receive) returns
    /// (the reactor treats `Timeout` as "loop again", `QueueClosed` as fatal).
    ///
    /// Unlike [`try_receive`](Agent::try_receive) it neither asserts a method
    /// nor auto-answers anything — the reactor's `default_react` owns the answer
    /// policy. A txn-owned §17.1.1.3 hop ACK (an armed obligation) is still
    /// absorbed below the API: it is the transaction layer's to claim, never
    /// the reactor's to see. A NORMAL ACK (to our own 2xx, no armed
    /// obligation) surfaces as `Inbound::Request` so the reactor records it.
    pub async fn recv_any(&self) -> Result<Inbound, StepError> {
        loop {
            match self.try_recv().await? {
                SipMessage::Request(r) => {
                    // An armed non-2xx hop ACK is the txn layer's; a plain 2xx
                    // ACK is not armed and surfaces (idempotent re-sight — the
                    // receive core already sighted it, `note_ack` is a no-op).
                    if self.ack_obligation_claims(&r) {
                        continue;
                    }
                    return Ok(Inbound::Request(ServerTxn::from_request(self.clone(), r)));
                }
                SipMessage::Response(r) => return Ok(Inbound::Response(r)),
            }
        }
    }

    /// Park until the §17.1.1.3 hop ACK for the given INVITE server transaction
    /// (`(Call-ID, top-Via branch)`) has been SIGHTED by the receive core —
    /// the non-pulling twin of [`ServerTxn::expect_ack`], for the reactive
    /// actor: its own `recv_any` claims the ACK below the API (never surfacing
    /// it), and this future is how the actor still observes the fulfilment
    /// (closing its `reject-final` ledger obligation). Never times out; run it
    /// as a bounded `select!` arm.
    pub(crate) async fn hop_ack_fulfilled(&self, call_id: &str, branch: &str) {
        self.txn.hop_ack_fulfilled(call_id, branch).await
    }

    /// Begin an out-of-dialog INVITE to `peer`. Returns a builder; call
    /// [`Invite::send`] (optionally after [`Invite::with_sdp`] / [`Invite::through`]).
    pub fn invite<'a>(&'a self, peer: &'a Agent) -> Invite<'a> {
        Invite::new(self, peer)
    }

    /// Begin a generic **out-of-dialog** request of any [`OutOfDialogMethod`]
    /// (OPTIONS, MESSAGE, SUBSCRIBE, …) addressed to `peer` — the any-method
    /// sibling of [`invite`](Agent::invite). The mechanical SIP layer (Via +
    /// fresh branch, From-tag, Call-ID, CSeq, Contact, Max-Forwards,
    /// Content-Type/Length) is auto-filled exactly like the INVITE path; the
    /// caller supplies only headers/body. Returns a builder; finish with the
    /// fallible [`OutOfDialogRequest::try_send`] (load lane) or the panicking
    /// [`OutOfDialogRequest::send`] (functional tests).
    ///
    /// For a dialog-CREATING INVITE keep using [`invite`](Agent::invite) — this
    /// builder tracks no dialog state (a non-INVITE out-of-dialog transaction
    /// creates none).
    pub fn request<'a>(
        &'a self,
        method: OutOfDialogMethod,
        peer: &'a Agent,
    ) -> OutOfDialogRequest<'a> {
        OutOfDialogRequest::new(self, peer, method)
    }

    /// Receive the next request and assert its method. Returns a UAS-side
    /// transaction handle for sending responses. Panicking veneer over
    /// [`try_receive`](Agent::try_receive).
    pub async fn receive(&self, method: &str) -> ServerTxn {
        unwrap_step(self.try_receive(method).await)
    }

    /// **Best-effort socket drain** — read (and discard) every datagram *currently
    /// queued* at this UA without waiting, asserting nothing about them. Each read
    /// goes through the recording layer, so a message the scenario delivered but
    /// never explicitly `receive`d (a relayed final response the test didn't await,
    /// a retransmit toward a deliberately-silent peer) is recorded as **received**
    /// rather than surfacing as "lost in transit" / a `queueLeak` at bind close.
    ///
    /// This models a real always-on UA: its kernel keeps reading the socket even
    /// after the application is done driving the call. Pair it with a clock pump
    /// (e.g. `FailoverHarness::linger_peers`) so in-flight datagrams first land in
    /// the queue, then drain. Returns the number of datagrams drained.
    pub async fn drain(&self) -> usize {
        let mut n = 0;
        while self.ep.try_recv().is_some() {
            n += 1;
        }
        n
    }

    /// The next TU-visible datagram already queued at this UA, WITHOUT waiting
    /// — `None` when nothing is pending. For a body that observes what a window
    /// it held open delivered (the rungs of a ladder the peer is still
    /// climbing) rather than expecting one message. Passes the absorption seam
    /// like every receive, so a §17.2 duplicate never surfaces here either.
    /// Panics on an unparseable datagram, as the functional lane does.
    pub async fn take_queued(&self) -> Option<SipMessage> {
        match self.take_held_or_queued().await? {
            Ok(msg) => Some(msg),
            Err(e) => panic!("{} received an unparseable datagram: {e}", self.name),
        }
    }

    /// Send an out-of-dialog REFER addressed to `dst` whose To carries a bogus
    /// tag and whose Request-URI carries a `callRef` the B2BUA never minted — so
    /// the router resolves the (non-existent) call, finds no state, and rejects
    /// it 481 (`maybe_reject_orphan`). Used by the out-of-dialog REFER reject
    /// scenario. Returns a client-transaction handle to `expect` the 481 on.
    pub async fn send_out_of_dialog_refer(&self, dst: SocketAddr, refer_to: &str) -> InDialogTxn {
        // A synthetic dialog the B2BUA has never seen: fresh Call-ID, a bogus
        // remote (To) tag, and a remote target carrying a bogus stamped callRef
        // (unreserved chars → no escaping needed; the router reads it verbatim),
        // so resolution succeeds but hydration fails → the orphan 481 path.
        let view = StackDialog {
            call_id: format!("orphan-{}-{}", self.name, self.ids.next()),
            local_tag: self.tag(),
            remote_tag: "bogus-refer-tag".into(),
            local_uri: self.uri.clone(),
            remote_uri: format!("sip:unknown@{}", dst.ip()),
            remote_target: format!(
                "sip:unknown@{}:{};callRef=w0-orphan-bogus;leg=b-1",
                dst.ip(),
                dst.port()
            ),
            local_cseq: 0,
            route_set: vec![],
        };
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.via()),
            contact: Some(self.contact()),
            extra_headers: vec![SipHeader { name: "Refer-To".into(), value: refer_to.into() }],
            ..Default::default()
        };
        let res = generate_in_dialog_request(InDialogMethod::Refer, &view, &opts);
        self.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn::new(
            self.clone(),
            // A REFER's finals take no ACK.
            None,
            dst,
        )
    }

    /// REGISTER this UA's AOR → its own Contact with a `registrar` front proxy,
    /// then wait for the 200 OK. A faithful mimic of a SIP UA's register step
    /// (RFC 3261 §10.2): the AOR is `aor` (the To/From URI), the Contact is this
    /// agent's `sip:name@ip:port`, and `ttl_sec` becomes the `Expires` the
    /// registrar grants. Returns the granted `Expires` (seconds) parsed back off
    /// the 200's `Expires` header, so a caller can assert / schedule a refresh
    /// (re-REGISTER) before it lapses. Out-of-dialog, no dialog is created.
    ///
    /// `aor` is the address-of-record URI (e.g. `sip:bob@example.com`); its
    /// userpart is what the registrar keys the binding on. Send `ttl_sec = 0`
    /// to de-register.
    pub async fn register(&self, registrar: SocketAddr, aor: &str, ttl_sec: u32) -> u32 {
        let call_id = format!("reg-{}-{}@{}", self.name, self.ids.next(), self.addr.ip());
        let opts = GenerateOutOfDialogRequestOpts {
            // The REGISTER Request-URI is the registrar (domain), not a user.
            request_uri: Some(Uri::sip(SipStr::owned(&registrar.ip().to_string()))),
            call_id,
            from: Some(from_of(aor, &self.tag())),
            to: Some(to_of(aor)),
            cseq: 1,
            via: Some(self.via()),
            // The Contact the registrar stores verbatim is this agent's wire
            // address (`sip:name@ip:port`) — the standard generated Contact.
            contact: Some(self.contact()),
            max_forwards: Some(70),
            body: vec![],
            content_type: None,
            // The requested binding lifetime (RFC 3261 §10.2.1.1).
            extra_headers: vec![SipHeader {
                name: "Expires".into(),
                value: ttl_sec.to_string().into(),
            }],
        };
        let req = generate_out_of_dialog_request(OutOfDialogMethod::Register, &opts);
        self.send(&SipMessage::Request(req), registrar).await;
        let resp = expect_response(self, 200, None).await;
        // Echo back the Expires the registrar actually granted (RFC 3261 §10.3
        // step 8): the registrar may clamp our request; the UA refreshes on it.
        resp.header::<header::Expires>()
            .and_then(Result::ok)
            .map(|expires| expires.value())
            .unwrap_or(ttl_sec)
    }
}
