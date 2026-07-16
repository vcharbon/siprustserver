//! [`Agent`] — the stateful fake UA: identity minting (branch/tag/Via/
//! Contact), the ONE fallible send/receive core (threaded through the §17.2
//! receive view), and the basic receive/dispatch primitives. The tolerant /
//! absorbing receive policies live in [`super::tolerant_recv`].

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use sip_message::generators::{
    generate_in_dialog_request, generate_out_of_dialog_request, generate_response, ContactSpec,
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, OutOfDialogMethod, SipTransport, StackDialog, ViaSpec,
};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use super::addressing::{top_via_addr, top_via_branch};
use super::client_txn::expect_response;
use super::dialog::InDialogTxn;
use super::harness::Ids;
use super::out_of_dialog::OutOfDialogRequest;
use super::rr_fold::RecordRouteFold;
use super::server_txn::ServerTxn;
use super::step::{unwrap_step, StepError};
use super::txn_view::{AckObligations, TxnVerdict, TxnView};
use super::Invite;

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
    /// §17.2 once-and-only-once receive view ([`TxnView`]). Shared across
    /// clones — one transaction table per logical UA.
    pub(crate) txn: Arc<TxnView>,
    /// §17.1.1.3 UAS-side ACK obligations ([`AckObligations`]). Shared across
    /// clones — one table per logical UA.
    pub(crate) acks: Arc<AckObligations>,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drop this UA to the **raw wire surface**: disable the §17.2
    /// once-and-only-once receive view ([`TxnView`]) so EVERY duplicate
    /// datagram surfaces again. Reach for this ONLY when retransmission is
    /// the *subject* of the test (Timer A/E assertions, ring-again pinning,
    /// drop-rate recovery) — the same sanction rule as
    /// [`Harness::allow_violation`](super::Harness::allow_violation). Affects
    /// every clone of this UA.
    pub fn wire_view(&self) {
        self.txn.wire.store(true, Ordering::Relaxed);
    }

    pub(super) fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    pub(super) fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    pub(super) fn via(&self) -> ViaSpec {
        ViaSpec {
            local_ip: self.addr.ip().to_string(),
            local_port: self.addr.port(),
            transport: SipTransport::Udp,
            branch: self.branch(),
            custom_params: vec![],
        }
    }
    /// A fresh top `Via` header value (new branch) — a new client transaction
    /// (RFC 3261 §8.1.1.7) for a resend (e.g. the §22.2 authenticated INVITE).
    pub(super) fn via_header(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.addr.ip(),
            self.addr.port(),
            self.branch()
        )
    }
    pub(super) fn contact(&self) -> ContactSpec {
        ContactSpec {
            user: self.name.clone(),
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            uri_params: vec![],
        }
    }

    /// Panicking veneer over [`try_send`](Agent::try_send).
    pub(super) async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        unwrap_step(self.try_send(msg, dst).await)
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
        self.ep
            .send_to(&serialize(msg), dst)
            .await
            .map_err(|e| StepError::Transport { who: self.name.clone(), detail: e.to_string() })
    }

    /// THE receive core: one SIP datagram surfaced through the §17.2 receive
    /// view ([`TxnView`]). A timeout / closed queue / parse error is a
    /// [`StepError`]; the functional lane panics on it via [`recv`](Agent::recv).
    ///
    /// Every surfaced ACK is also SIGHTED against the §17.1.1.3 obligation
    /// table ([`AckObligations`]) — fulfilment is recorded here so it happens
    /// on ANY pull path, but the absorb decision stays with the caller
    /// ([`ack_obligation_claims`](Agent::ack_obligation_claims) at the
    /// would-be-error sites), so an explicit `receive("ACK")` keeps working.
    pub(super) async fn try_recv(&self) -> Result<SipMessage, StepError> {
        loop {
            let pkt = match tokio::time::timeout(self.recv_timeout, self.ep.recv()).await {
                Err(_) => return Err(StepError::Timeout { who: self.name.clone() }),
                Ok(None) => return Err(StepError::QueueClosed { who: self.name.clone() }),
                Ok(Some(p)) => p,
            };
            let msg = CustomParser::new().parse(&pkt.raw).map_err(|e| StepError::Unparseable {
                who: self.name.clone(),
                detail: e.to_string(),
            })?;
            if let SipMessage::Request(r) = &msg {
                self.ack_obligation_claims(r);
            }
            match self.txn.verdict(&pkt.raw, &msg) {
                TxnVerdict::Surface => return Ok(msg),
                TxnVerdict::Absorb => continue,
            }
        }
    }

    /// Whether `r` is the hop ACK of an armed §17.1.1.3 obligation on this UA.
    /// Marks the obligation fulfilled (idempotent). A receive path that would
    /// otherwise ERROR on an unexpected ACK calls this and absorbs instead —
    /// the ACK-races-the-next-INVITE interleave, in either order, never trips
    /// a body.
    pub(crate) fn ack_obligation_claims(&self, r: &SipRequest) -> bool {
        r.method.as_str() == "ACK"
            && top_via_branch(&r.headers).is_some_and(|b| self.acks.note_ack(&r.call_id, &b))
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
                    if r.method != method {
                        if self.ack_obligation_claims(&r) {
                            continue;
                        }
                        return Err(StepError::WrongMethod {
                            who: self.name.clone(),
                            expected: method.to_string(),
                            got: r.method.to_string(),
                        });
                    }
                    return Ok(ServerTxn::from_request(self.clone(), r));
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected a {method} request",
                            r.status, r.reason
                        ),
                    })
                }
            }
        }
    }

    /// Receive the next inbound message of EITHER kind through the shared §17.2
    /// receive view ([`TxnView`]) — the reactive-actor primitive (the
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
        self.acks.fulfilled(call_id, branch).await
    }

    /// Best-effort drain-and-200 for the load driver's teardown: for up to
    /// `window`, receive any inbound request and answer it `200 OK` (Via-routed),
    /// then return when the window elapses or the socket goes quiet. After a
    /// failed call's a-leg has been BYE'd, this lets the in-process callee answer
    /// the SUT's relayed b-leg BYE so the SUT closes its b-leg promptly instead of
    /// waiting out a retransmit Timer. Never panics (sends are best-effort).
    pub async fn quiesce(&self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, self.ep.recv()).await {
                Ok(Some(pkt)) => {
                    if let Ok(SipMessage::Request(r)) = CustomParser::new().parse(&pkt.raw) {
                        let resp =
                            generate_response(&r, 200, "OK", &GenerateResponseOpts::default());
                        let dst = top_via_addr(&r).unwrap_or(self.addr);
                        let _ = self.try_send(&SipMessage::Response(resp), dst).await;
                    }
                }
                _ => return, // timed out or queue closed
            }
        }
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
    pub fn request<'a>(&'a self, method: OutOfDialogMethod, peer: &'a Agent) -> OutOfDialogRequest<'a> {
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

    /// Send an out-of-dialog REFER addressed to `dst` whose To carries a bogus
    /// tag and whose Request-URI carries a `callRef` the B2BUA never minted — so
    /// the router resolves the (non-existent) call, finds no state, and rejects
    /// it 481 (`maybe_reject_orphan`). Used by the out-of-dialog REFER reject
    /// scenario. Returns a client-transaction handle to `expect` the 481 on.
    pub async fn send_out_of_dialog_refer(
        &self,
        dst: SocketAddr,
        refer_to: &str,
    ) -> InDialogTxn {
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
            extra_headers: vec![SipHeader {
                name: "Refer-To".into(),
                value: refer_to.into(),
            }],
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
            request_uri: format!("sip:{}", registrar.ip()),
            call_id,
            from_uri: aor.to_string(),
            from_tag: self.tag(),
            to_uri: aor.to_string(),
            to_tag: None,
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
                value: ttl_sec.to_string(),
            }],
        };
        let req = generate_out_of_dialog_request(OutOfDialogMethod::Register, &opts);
        self.send(&SipMessage::Request(req), registrar).await;
        let resp = expect_response(self, 200, None).await;
        // Echo back the Expires the registrar actually granted (RFC 3261 §10.3
        // step 8): the registrar may clamp our request; the UA refreshes on it.
        get_header(&resp.headers, "expires")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(ttl_sec)
    }
}
