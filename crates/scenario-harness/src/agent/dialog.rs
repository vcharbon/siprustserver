//! In-dialog requests: the confirmed [`Dialog`], the [`InDialogRequest`]
//! builder (early + confirmed dialogs, per-fork CSeq), the generic
//! [`InDialogTxn`] client transaction, and the CANCELlable [`ClientReinvite`].

use std::collections::HashMap;
use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_2xx, generate_in_dialog_request, GenerateAckFor2xxOpts,
    GenerateInDialogRequestOpts, InDialogMethod, StackDialog,
};
use sip_message::{
    apply_name_forms, apply_remote_target_emits, CseqDeviation, CseqPattern, EmitOpts,
    MessageTemplate, SipHeader, SipMessage, SipRequest, SipResponse,
};

use super::addressing::next_hop;
use super::client_txn::{
    try_expect_response, try_expect_response_tolerating, try_send_cancel, AckCtx,
};
use super::step::{unwrap_step, StepError};
use super::Agent;

/// A confirmed dialog. In-dialog requests auto-increment CSeq and route to the
/// remote target. `Clone` so the load driver can snapshot it into its teardown
/// scope (each clone shares the `Arc` endpoint and carries the dialog state
/// needed to BYE).
#[derive(Clone)]
pub struct Dialog {
    agent: Agent,
    fallback_addr: SocketAddr,
    dialog: StackDialog,
    /// A declared CSeq relative-pattern deviation for this dialog's outbound
    /// in-dialog requests (U5). `None` = the stack's base numbering is
    /// authoritative (zero-deviation dialogs are byte-identical to pre-U5).
    cseq_dev: Option<CseqDeviation>,
}

impl Dialog {
    pub(super) fn new(agent: Agent, fallback_addr: SocketAddr, dialog: StackDialog) -> Self {
        Dialog { agent, fallback_addr, dialog, cseq_dev: None }
    }

    /// Declare a CSeq relative-pattern deviation for this dialog's outbound
    /// in-dialog requests (offset / jump / reuse relative to the stack's base).
    /// Captured out-of-pattern CSeq anomalies replay without freezing absolute
    /// numbers. ACK/CANCEL are unaffected (they reuse the related request's
    /// CSeq, RFC 3261 §12.2.1.1).
    pub fn set_cseq_pattern(&mut self, pattern: CseqPattern) {
        self.cseq_dev = Some(CseqDeviation::new(pattern));
    }

    /// Builder form of [`set_cseq_pattern`](Self::set_cseq_pattern).
    pub fn with_cseq_pattern(mut self, pattern: CseqPattern) -> Self {
        self.set_cseq_pattern(pattern);
        self
    }

    /// Set the dialog's local CSeq floor — the next in-dialog request uses
    /// `v + 1`. RFC 3261 §12.2.1.1 lets a UA pick ANY initial sequence number
    /// for its own CSeq space, so a test can deliberately align this side's
    /// CSeq with the peer's to force `(Call-ID, CSeq, method)`-coincident
    /// crossing transactions (e.g. BYE/BYE glare through a relay, where only
    /// the top-Via branch disambiguates the two 200s — RFC 3261 §17.1.3).
    pub fn set_local_cseq(&mut self, v: u32) {
        self.dialog.local_cseq = v;
    }

    /// This side's current CSeq high-water — the number the LAST in-dialog
    /// request this UA sent used (0 before any). The reactive actor reads it
    /// right after [`bye`](Self::bye) / [`request`](Self::request) to key the
    /// ledger obligation the sent request opens (the matching final response
    /// carries the same `CSeq` number).
    pub fn local_cseq(&self) -> u32 {
        self.dialog.local_cseq
    }

    /// This side's dialog tag (the To-tag a UAS minted on its 2xx / the
    /// From-tag a UAC sent). Read-only; lets a test re-answer an INVITE
    /// retransmission with the SAME tag (a faithful RFC 3261 §17.2.1 2xx
    /// retransmission via `Respond::with_to_tag`) instead of minting a
    /// phantom fork dialog.
    pub fn local_tag(&self) -> &str {
        &self.dialog.local_tag
    }

    /// The dialog's REMOTE tag (the peer's To-tag as a UAC / From-tag as a
    /// UAS). Lets a fork-aware caller compare a late 2xx's To-tag against the
    /// CONFIRMED (winner) dialog's — a mismatch identifies a losing fork's
    /// late 200 (RFC 3261 §13.2.2.4).
    pub fn remote_tag(&self) -> &str {
        &self.dialog.remote_tag
    }

    /// Send a BYE (CSeq auto-incremented). Returns its client transaction.
    pub async fn bye(&mut self) -> InDialogTxn {
        self.request(InDialogMethod::Bye, None).await
    }

    /// Best-effort BYE for the load driver's teardown: builds and sends the BYE
    /// (advancing the dialog CSeq so it is valid against the SUT), swallowing any
    /// transport error and **not** waiting for the 200. Runs on a failed call to
    /// release the dialog on the SUT (RFC 3261 §15) so no call is leaked.
    pub async fn bye_best_effort(&mut self) {
        let _ = self.send_request(InDialogMethod::Bye).try_send().await;
    }

    /// ACK a re-INVITE's 2xx on this confirmed dialog (RFC 3261 §13.2.2.4 — the
    /// ACK echoes the re-INVITE's CSeq, which `request(INVITE, …)` left as the
    /// dialog's `local_cseq`). Carries an optional SDP answer (the delayed-offer
    /// case where the answer rides the ACK, RFC 3264 §4). Routed to the next hop
    /// like any in-dialog request; the B2BUA relays it end-to-end. The ACK is a
    /// dedicated primitive: it takes no template in v1 (a captured ACK's
    /// frozen-header quirks are not replayable yet).
    pub async fn ack(&mut self, sdp: Option<&str>) {
        self.ack_for(self.dialog.local_cseq, sdp).await;
    }

    /// ACK a re-INVITE's 2xx echoing an **explicit** INVITE CSeq (RFC 3261
    /// §13.2.2.4 — the ACK number is the re-INVITE's, taken from the 2xx being
    /// ACKed, NOT this dialog's current `local_cseq`, which may have advanced past
    /// it if another in-dialog request went out meanwhile). Idempotent and
    /// re-derivable from the confirmed dialog + the response's CSeq, so a
    /// retransmitted 2xx can always be re-ACKed with no per-call one-shot state
    /// (mirrors the loadgen mux's `(Call-ID, CSeq)` re-ACK contract). Carries an
    /// optional SDP answer (the delayed-offer case, RFC 3264 §4).
    pub async fn ack_for(&mut self, invite_cseq: u32, sdp: Option<&str>) {
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            cseq: Some(invite_cseq),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(None, &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
    }

    /// Send any in-dialog request (re-INVITE, INFO, …); attach an SDP body
    /// with `sdp`. Sugar over the [`send_request`](Dialog::send_request)
    /// builder — same mechanics (CSeq bump, next-hop routing, §17.1.1.3
    /// (re-)INVITE retention on the returned transaction).
    pub async fn request(&mut self, method: InDialogMethod, sdp: Option<&str>) -> InDialogTxn {
        let mut req = self.send_request(method);
        if let Some(s) = sdp {
            req = req.with_sdp(s);
        }
        req.send().await
    }

    /// Begin an in-dialog request with fine-grained control over RAck (RFC 3262
    /// PRACK) and arbitrary extra headers. Returns a builder; call
    /// [`InDialogRequest::send`]. Use this over [`request`](Dialog::request) when
    /// the request needs an `RAck` header (PRACK) or other custom headers.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        let agent = self.agent.clone();
        let fallback = self.fallback_addr;
        // Disjoint field borrows: the dialog state and the CSeq deviation.
        let dev = self.cseq_dev.as_mut();
        InDialogRequest::new(agent, &mut self.dialog, fallback, method).with_cseq_dev(dev)
    }

    /// Emit an in-dialog request from a captured [`MessageTemplate`] on this
    /// confirmed dialog. The request method is taken from the template's start
    /// line; the stack regenerates the tier-1 fields (Via + branch, the reused
    /// Call-ID / From-tag / To-tag, the next CSeq, Max-Forwards, Content-Length,
    /// Contact, Request-URI + Route set) while every frozen header rides verbatim.
    /// Panics if the template is not a request of an admissible in-dialog method:
    /// ACK/CANCEL are dedicated primitives that take no template in v1 (their
    /// captured frozen-header quirks are not replayable yet), and out-of-dialog-
    /// only methods are rejected. See [`EmitOpts`] for the v1 header-order limitation.
    pub async fn send_template(&mut self, tmpl: &MessageTemplate, opts: EmitOpts) -> InDialogTxn {
        let method = tmpl
            .method()
            .and_then(|m| InDialogMethod::try_from(m).ok())
            .unwrap_or_else(|| {
                panic!("Dialog::send_template requires an in-dialog request template, got {:?}", tmpl.start())
            });
        self.send_request(method).template(tmpl, opts).send().await
    }

    /// Send an in-dialog **re-INVITE** (optional SDP offer) and keep the
    /// transaction handle, so the renegotiation can later be CANCELled — the
    /// in-dialog mirror of [`ClientInvite::cancel`](super::ClientInvite::cancel)
    /// (RFC 3261 §9.1). Mechanics are identical to
    /// [`request`](Dialog::request)`(InDialogMethod::Invite, …)` (same CSeq bump
    /// on this dialog, same next-hop routing); only the returned handle differs:
    /// [`ClientReinvite`] can `expect(…)` responses *and*
    /// [`cancel`](ClientReinvite::cancel) the still-pending re-INVITE.
    pub async fn reinvite(&mut self, sdp: Option<&str>) -> ClientReinvite {
        let mut builder = self.send_request(InDialogMethod::Invite);
        if let Some(s) = sdp {
            builder = builder.with_sdp(s);
        }
        let (_txn, request) = unwrap_step(builder.try_send_with_request().await);
        ClientReinvite {
            agent: self.agent.clone(),
            wire_dst: next_hop(&self.dialog, self.fallback_addr),
            original_invite: request,
        }
    }
}

/// Client transaction for an in-dialog re-INVITE sent via [`Dialog::reinvite`].
/// Like [`InDialogTxn`] it can await responses; additionally it holds the
/// re-INVITE as sent (plus its wire destination), so the pending renegotiation
/// can be CANCELled (RFC 3261 §9.1) — the in-dialog counterpart of
/// [`ClientInvite::cancel`](super::ClientInvite::cancel).
pub struct ClientReinvite {
    agent: Agent,
    wire_dst: SocketAddr,
    original_invite: SipRequest,
}

impl ClientReinvite {
    /// The §17.1.1.3 auto-ACK context for this re-INVITE transaction.
    fn ack_ctx(&self) -> AckCtx<'_> {
        AckCtx {
            agent: &self.agent,
            invite: &self.original_invite,
            wire_dst: self.wire_dst,
        }
    }

    /// Wait for and assert a response status (the relayed 1xx/2xx/487 for this
    /// re-INVITE, or the 200 to a CANCEL — whichever arrives next). A non-2xx
    /// final to the re-INVITE is auto-ACKed on its branch (RFC 3261
    /// §17.1.1.3), like [`ClientInvite::expect`](super::ClientInvite::expect).
    /// Panicking veneer over [`try_expect`](ClientReinvite::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// Fallible [`expect`](ClientReinvite::expect).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        try_expect_response(&self.agent, status, Some(&self.ack_ctx())).await
    }

    /// CANCEL this still-pending re-INVITE (RFC 3261 §9.1) — the in-dialog
    /// mirror of [`ClientInvite::cancel`](super::ClientInvite::cancel). The
    /// CANCEL reuses the re-INVITE's Request-URI / Call-ID / From / To / topmost
    /// Via branch, echoes its Route set, uses the re-INVITE's CSeq *number* with
    /// method `CANCEL`, and goes to the SAME wire destination the re-INVITE
    /// took. Returns a client transaction to `expect(200)` on; the matching
    /// `487 Request Terminated` for the re-INVITE arrives on this same UA via
    /// [`ClientReinvite::expect`]. Per §9, this ends only the renegotiation —
    /// the established dialog (and the call through a B2BUA) must survive. CANCEL
    /// is a dedicated primitive: it takes no template in v1 (a captured CANCEL's
    /// frozen-header quirks are not replayable yet).
    pub async fn cancel(&self) -> InDialogTxn {
        unwrap_step(try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await);
        InDialogTxn::new(
            self.agent.clone(),
            // The CANCELled re-INVITE's 487 is read — and auto-ACKed — via
            // [`ClientReinvite::expect`]; the CANCEL's own finals take no ACK.
            None,
            self.wire_dst,
        )
    }
}

/// Builder for an in-dialog request carrying an `RAck` and/or custom headers
/// (the PRACK path, RFC 3262 §7.2). Borrows the originating dialog's
/// [`StackDialog`] so the CSeq bump persists — works over both a confirmed
/// [`Dialog`] and an early [`ClientInvite`](super::ClientInvite) (PRACK
/// precedes the final 2xx/ACK).
pub struct InDialogRequest<'a> {
    agent: Agent,
    dialog: &'a mut StackDialog,
    fallback: SocketAddr,
    method: InDialogMethod,
    body: Vec<u8>,
    content_type: Option<String>,
    rack: Option<String>,
    extra_headers: Vec<SipHeader>,
    /// A template body carried NO Content-Type: suppress the generator's default
    /// `application/sdp` stamp (see [`Invite::template`](super::Invite::template)).
    suppress_default_ct: bool,
    /// `(canonical, wire)` name-forms for stack-regenerated headers the capture
    /// wrote compact — applied to the WIRE copy only.
    name_forms: Vec<(String, String)>,
    /// `(canonical, captured_value)` remote-target headers (Contact) — captured
    /// user + params over the bound socket's host:port; WIRE copy only.
    remote_emits: Vec<(String, String)>,
    to_tag: Option<String>,
    /// Per-fork CSeq map (see `ClientInvite`'s fork tracking). Present only on
    /// the early-dialog path; when a `with_to_tag` fork is addressed the CSeq
    /// comes from this independent per-fork sequence, not the shared dialog
    /// counter.
    fork_cseq: Option<&'a mut HashMap<String, u32>>,
    /// The dialog's declared CSeq deviation (U5). Present only on the confirmed-
    /// dialog path (never the forked early-dialog path); overrides the natural
    /// CSeq for this request per the declared pattern.
    cseq_dev: Option<&'a mut CseqDeviation>,
}

impl<'a> InDialogRequest<'a> {
    pub(super) fn new(
        agent: Agent,
        dialog: &'a mut StackDialog,
        fallback: SocketAddr,
        method: InDialogMethod,
    ) -> Self {
        InDialogRequest {
            agent,
            dialog,
            fallback,
            method,
            body: vec![],
            content_type: None,
            rack: None,
            extra_headers: vec![],
            suppress_default_ct: false,
            name_forms: vec![],
            remote_emits: vec![],
            to_tag: None,
            fork_cseq: None,
            cseq_dev: None,
        }
    }

    /// Wire in the originating `ClientInvite`'s per-fork CSeq map so a
    /// `with_to_tag` request uses that fork's independent sequence.
    pub(super) fn with_fork_cseq(mut self, map: &'a mut HashMap<String, u32>) -> Self {
        self.fork_cseq = Some(map);
        self
    }

    /// Wire in the confirmed dialog's declared CSeq deviation (U5); `None` is a
    /// no-op (the stack's base numbering stays authoritative).
    pub(super) fn with_cseq_dev(mut self, dev: Option<&'a mut CseqDeviation>) -> Self {
        self.cseq_dev = dev;
        self
    }

    /// Address this request to a specific early dialog by overriding the remote
    /// (To) tag — the per-fork PRACK (RFC 3262 §5). On the early-dialog path
    /// the request then rides that fork's OWN CSeq sequence (seeded from the
    /// INVITE's CSeq), leaving the shared counter untouched.
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach an SDP body (e.g. the answer carried in a PRACK to a delayed
    /// offer, RFC 3264 §4). Thin sugar over [`with_body`](Self::with_body) that
    /// pins `Content-Type: application/sdp`.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.body = sdp.as_bytes().to_vec();
        self.content_type = Some("application/sdp".to_string());
        self
    }

    /// Attach an arbitrary-MIME body — mirror of [`with_sdp`](Self::with_sdp)
    /// for any `Content-Type` and raw (binary-safe) bytes. Drives an in-dialog
    /// `INFO`/other with a real payload: an `application/orangeindata` SUP body,
    /// a `multipart/mixed` dual-part body, a `User-To-User` INFO, etc. With
    /// non-empty `bytes` the request carries `Content-Type` + `Content-Length`;
    /// an empty `bytes` emits no `Content-Type` (the generator only stamps it for
    /// a non-empty body) — for a bodyless typed header use `with_header`.
    pub fn with_body(mut self, content_type: &str, bytes: Vec<u8>) -> Self {
        self.body = bytes;
        self.content_type = Some(content_type.to_string());
        self
    }

    /// Set the `RAck` header (`<rseq> <cseq> <method>`, RFC 3262 §7.2).
    pub fn with_rack(mut self, rack: &str) -> Self {
        self.rack = Some(rack.to_string());
        self
    }

    /// Emit this in-dialog request from a captured [`MessageTemplate`]: freeze the
    /// template's non-tier-1 headers (verbatim value/casing/duplicate layout) and
    /// its body, leaving the stack to regenerate the dialog-critical fields. The
    /// captured `Content-Type` rides as a frozen header. See [`Dialog::send_template`]
    /// for the ergonomic method-deriving entry point.
    pub fn template(mut self, tmpl: &MessageTemplate, opts: EmitOpts) -> Self {
        // v1: casing + duplicate-header layout are always preserved for frozen
        // headers; `preserve_order` requests nothing further yet (see EmitOpts).
        let EmitOpts { preserve_order: _ } = opts;
        let frozen = tmpl.frozen_headers();
        // Intentionally LITERAL (not compact-aware): a frozen compact `c:` does
        // not match "content-type" here, so suppress=true and the generator's
        // added full Content-Type default is stripped below while the frozen `c:`
        // survives (a compact-aware probe would wrongly leave both).
        self.suppress_default_ct =
            !frozen.iter().any(|h| h.name.eq_ignore_ascii_case("content-type"));
        // Append AFTER any prior `with_header` entries — never drop them.
        self.extra_headers.extend(frozen);
        self.body = tmpl.body().to_vec();
        self.content_type = None;
        self.name_forms = tmpl.regenerated_name_forms();
        self.remote_emits = tmpl.remote_target_emits();
        self
    }

    /// Attach an arbitrary extra header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Generate and send the request; returns its client transaction. Panics on
    /// a transport failure — use [`try_send`](Self::try_send) in the load lane.
    pub async fn send(self) -> InDialogTxn {
        unwrap_step(self.try_send().await)
    }

    /// Fallible [`send`](Self::send) — the generic any-method in-dialog send for
    /// the load lane: a transport failure surfaces as [`StepError::Transport`]
    /// instead of a panic. The mechanical layer (Via/branch, CSeq bump, tags,
    /// route set) is identical.
    pub async fn try_send(mut self) -> Result<InDialogTxn, StepError> {
        self.try_send_inner().await.map(|(txn, _)| txn)
    }

    /// [`try_send`](Self::try_send) that also returns the request as sent —
    /// for tagging a message ANCHOR on a request no test agent receives (the
    /// REFER whose receiver is the SUT; see `CallCtx::anchor_sent`). The common
    /// path pays nothing for it (`try_send` discards the clone-free original).
    pub async fn try_send_with_request(mut self) -> Result<(InDialogTxn, SipRequest), StepError> {
        self.try_send_inner().await
    }

    async fn try_send_inner(&mut self) -> Result<(InDialogTxn, SipRequest), StepError> {
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.agent.via()),
            contact: Some(self.agent.contact()),
            body: std::mem::take(&mut self.body),
            content_type: self.content_type.take(),
            rack: self.rack.take(),
            extra_headers: std::mem::take(&mut self.extra_headers),
            ..Default::default()
        };
        // Per-fork addressing: generate against a dialog view with the chosen
        // remote tag. For a forked early dialog (a `with_to_tag` other than the
        // shared dialog's own tag) the CSeq rides that fork's OWN sequence
        // (seeded from the INVITE's CSeq), not the shared counter — RFC 3261
        // §12.2.1.1: each early dialog increments independently.
        let mut view = self.dialog.clone();
        let mut opts = opts;
        // On the early-dialog path EVERY explicit `with_to_tag` addresses a
        // distinct forked early dialog (RFC 3262 §5), so it rides that fork's OWN
        // CSeq sequence (seeded from the INVITE's CSeq) — NOT the shared counter,
        // which a sibling fork's PRACK must not perturb.
        let forked = self.to_tag.is_some() && self.fork_cseq.is_some();
        if let (Some(tag), Some(map)) = (self.to_tag.as_ref(), self.fork_cseq.as_deref_mut()) {
            view.remote_tag = tag.clone();
            // Seed from the INVITE's CSeq (the dialog's current local_cseq) the
            // first time this fork is addressed, then advance by one per request.
            let entry = map.entry(tag.clone()).or_insert(self.dialog.local_cseq);
            *entry += 1;
            opts.cseq = Some(*entry);
        } else if let Some(t) = &self.to_tag {
            view.remote_tag = t.clone();
        }
        // U5: a declared CSeq deviation overrides the natural number for this
        // confirmed-dialog request (never on the forked early-dialog path). The
        // shared-counter update below picks up the deviated value from `res`.
        if !forked {
            if let Some(dev) = self.cseq_dev.as_deref_mut() {
                if !dev.is_identity() {
                    opts.cseq = Some(dev.next_cseq(self.dialog.local_cseq));
                }
            }
        }
        let mut res = generate_in_dialog_request(self.method, &view, &opts);
        if self.suppress_default_ct {
            res.request.headers = sip_message::message_helpers::remove_header(
                &res.request.headers,
                "content-type",
            );
        }
        // Advance the SHARED dialog counter only for a non-forked request (a
        // forked request advanced its own per-fork entry above and must leave the
        // shared sequence untouched).
        if !forked {
            self.dialog.local_cseq = res.dialog.local_cseq;
        }
        let dst = next_hop(self.dialog, self.fallback);
        // Send a WIRE copy carrying the captured compact names (Via/From/…);
        // the canonical `request` is retained/returned so the §17.1.1.3 ACK's
        // header lookups (get_header, not compact-aware) still resolve.
        let request = res.request;
        let mut wire = request.clone();
        wire.headers = apply_name_forms(&request.headers, &self.name_forms);
        wire.headers = apply_remote_target_emits(&wire.headers, &self.remote_emits);
        self.agent.try_send(&SipMessage::Request(wire), dst).await?;
        Ok((
            InDialogTxn::new(
                self.agent.clone(),
                // A re-INVITE's non-2xx final takes a txn-layer ACK
                // (§17.1.1.3) — retain the request so the txn can build it.
                matches!(self.method, InDialogMethod::Invite).then(|| request.clone()),
                dst,
            ),
            request,
        ))
    }
}

/// Client transaction for an in-dialog request.
///
/// For a **re-INVITE** it retains the request as sent (plus its wire
/// destination), so a 3xx–6xx final — a 488 to a rejected renegotiation, a 491
/// glare — is auto-ACKed on the re-INVITE's branch/CSeq (RFC 3261 §17.1.1.3),
/// exactly like [`ClientInvite`](super::ClientInvite)'s finals. Non-INVITE
/// transactions (BYE, INFO, CANCEL, …) retain nothing: their finals take no ACK.
pub struct InDialogTxn {
    agent: Agent,
    /// The sent request, retained ONLY when it was an (re-)INVITE — the
    /// §17.1.1.3 non-2xx auto-ACK needs its Via branch / CSeq / R-URI / Route.
    invite: Option<SipRequest>,
    /// Where the request was sent (the next hop) — the non-2xx ACK is
    /// hop-by-hop and follows the SAME path.
    wire_dst: SocketAddr,
}

impl InDialogTxn {
    pub(super) fn new(agent: Agent, invite: Option<SipRequest>, wire_dst: SocketAddr) -> Self {
        InDialogTxn { agent, invite, wire_dst }
    }

    /// The §17.1.1.3 auto-ACK context — `None` for a non-INVITE transaction.
    fn ack_ctx(&self) -> Option<AckCtx<'_>> {
        self.invite.as_ref().map(|invite| AckCtx {
            agent: &self.agent,
            invite,
            wire_dst: self.wire_dst,
        })
    }

    /// Hop-ACK a non-2xx final to a re-INVITE THIS transaction sent (RFC 3261
    /// §17.1.1.3) — a `491 Request Pending` glare reject (§14.1), a 488. A no-op
    /// for a non-INVITE transaction or a non-matching response. The reactive
    /// actor's glare path needs this because `recv_any` surfaces the 491 as a
    /// bare response (it does NOT auto-ACK, unlike the blocking `try_expect`),
    /// yet the peer's server transaction still requires the hop ACK.
    pub async fn ack_non_2xx(&self, resp: &SipResponse) -> Result<(), StepError> {
        match self.ack_ctx() {
            Some(ctx) => ctx.ack_non_2xx(resp).await,
            None => Ok(()),
        }
    }

    /// Wait for and assert a response status. Panicking veneer over
    /// [`try_expect`](InDialogTxn::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// THE expect core for this transaction: a wrong status / timeout /
    /// unexpected request becomes a [`StepError`] (the functional lane panics
    /// on it via [`expect`](InDialogTxn::expect)).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        try_expect_response(&self.agent, status, self.ack_ctx().as_ref()).await
    }

    /// Fallible, tolerant [`try_expect`](InDialogTxn::try_expect): while waiting
    /// for the response, 200-OK any inbound request whose method is in
    /// `tolerate` (e.g. a `NOTIFY` that races ahead of the REFER's 202 on the
    /// same socket) and keep waiting — instead of mis-classifying the reorder.
    /// A real load tool faces UDP reordering, so this is the production-correct
    /// behaviour, not just a fake-fabric workaround.
    pub async fn try_expect_tolerating(
        &mut self,
        status: u16,
        tolerate: &[&str],
    ) -> Result<SipResponse, StepError> {
        try_expect_response_tolerating(&self.agent, status, tolerate, self.ack_ctx().as_ref())
            .await
    }

    /// Like [`expect`](InDialogTxn::expect), but first drains (and 200-OKs) any
    /// inbound requests whose method is in `tolerate` — the response-side analog
    /// of [`Agent::receive_tolerating`]. Under a paused clock a keepalive OPTIONS
    /// retransmit can race the awaited response on the same socket; tolerate it
    /// rather than relax the assertion (CLAUDE.md retransmit hazard).
    /// Panicking veneer over [`try_expect_tolerating`](InDialogTxn::try_expect_tolerating).
    pub async fn expect_tolerating(&mut self, status: u16, tolerate: &[&str]) -> SipResponse {
        unwrap_step(self.try_expect_tolerating(status, tolerate).await)
    }
}
