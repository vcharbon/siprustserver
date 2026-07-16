//! The UAC-side [`ClientInvite`] transaction for an initial INVITE (dialog
//! learning, forking, PRACK, §22.2 auth retry, §17.1.1.3 auto-ACK) and the
//! Send [`CancelHandle`]. The [`Invite`](super::Invite) builder that creates
//! it lives in [`super::invite`].

use std::collections::HashMap;
use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_2xx, GenerateAckFor2xxOpts, InDialogMethod,
    InviteClientTransactionHandle, StackDialog,
};
use sip_message::message_helpers::{get_headers, set_header};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipHeader, SipMessage, SipParser, SipRequest, SipResponse};

use super::client_txn::{try_expect_response, try_send_cancel, AckCtx};
use super::dialog::{Dialog, InDialogRequest, InDialogTxn};
use super::extract::{first_contact_uri, next_hop, rack_for};
use super::step::{unwrap_step, StepError};
use super::Agent;
use crate::realcall::auth::{parse_challenge, ChallengeResponder};

/// What a response fed to [`ClientInvite::absorb_response`] means for the
/// INVITE transaction it belongs to — the reactor's caller-side dispatch. The
/// `status` payloads are for the reactor's diagnostics/phase decisions.
#[allow(dead_code)]
pub(crate) enum InviteResponseFate {
    /// A provisional (learned into the early dialog); keep waiting.
    Provisional { status: u16 },
    /// A 2xx — the dialog is confirmed; the caller must now [`ClientInvite::ack`].
    Answered,
    /// A non-2xx final (auto-ACKed on arrival, §17.1.1.3); the INVITE failed.
    Failed { status: u16 },
}

/// UAC-side INVITE client transaction + the dialog it is establishing.
/// Constructed by [`Invite::send`](super::Invite::send) (fields `pub(super)`
/// for that builder).
pub struct ClientInvite {
    pub(super) agent: Agent,
    /// Where to send the ACK if no Contact was learned (shouldn't happen for a
    /// well-behaved 2xx, but keeps the harness robust).
    pub(super) fallback_addr: SocketAddr,
    /// The wire destination the INVITE was actually sent to (the proxy/B2BUA when
    /// [`Invite::through`](super::Invite::through) was used, else the peer). A
    /// CANCEL for a still-pending INVITE must follow the SAME path (RFC 3261
    /// §9.1), so it is retained here.
    pub(super) wire_dst: SocketAddr,
    pub(super) original_invite: SipRequest,
    pub(super) dialog: StackDialog,
    /// Per-forked-early-dialog CSeq (keyed by the fork's To-tag), for the
    /// delayed-offer forking case (RFC 3261 §12.1.2 / §12.2.1.1): one INVITE
    /// creates several early dialogs that each carry an INDEPENDENT CSeq space
    /// seeded from the INVITE's CSeq, so both forks' first PRACKs are
    /// `INVITE_CSeq + 1`. Without this the single shared counter makes each fork's
    /// PRACK (and the later BYE) non-contiguous within its dialog — which the
    /// per-dialog RFC 3261 §12.2.1.1 audit (correctly) rejects. Empty until a
    /// `with_to_tag` request fork is addressed.
    pub(super) fork_cseq: HashMap<String, u32>,
}

impl ClientInvite {
    /// Wait for and assert a response status. Learns the remote tag (from the
    /// first tagged response) and the remote target (from Contact), so the
    /// later ACK/BYE route and address correctly. Returns the response.
    ///
    /// **Txn-layer auto-ACK (RFC 3261 §17.1.1.3):** any non-2xx final this
    /// transaction surfaces (here and in the `try_*` siblings) is ACKed
    /// automatically on the INVITE's branch, completing the client transaction
    /// the way a real txn layer does. A test whose *subject* is the
    /// ACK-retransmission dance hand-rolls raw sends instead.
    ///
    /// Panicking veneer over [`try_expect`](ClientInvite::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// THE expect core for this INVITE transaction: a wrong status / timeout /
    /// unexpected request becomes a [`StepError`] (the functional lane panics
    /// on it via [`expect`](ClientInvite::expect)). On success it learns the
    /// dialog state (remote tag / target / route set).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        let resp = try_expect_response(&self.agent, status, Some(&self.ack_ctx())).await?;
        self.learn_from_response(&resp);
        Ok(resp)
    }

    /// The §17.1.1.3 auto-ACK context for THIS INVITE transaction (tracks the
    /// auth-retried INVITE — `ack_and_resend_with_auth` re-points
    /// `original_invite`, so a final to the retried transaction matches).
    fn ack_ctx(&self) -> AckCtx<'_> {
        AckCtx {
            agent: &self.agent,
            invite: &self.original_invite,
            wire_dst: self.wire_dst,
        }
    }

    /// SIPp-`optional` semantics for the load lane: wait for the FINAL
    /// response `status`, absorbing — and collecting — any interleaved
    /// provisional (1xx), instead of erroring on it the way
    /// [`try_expect`](Self::try_expect) does. A multi-leg shape legitimately
    /// sees a relay-timing-dependent number of provisionals (the reroute
    /// caller gets one or two 180s depending on when the alternate leg rings),
    /// so a body says `try_expect_final(200)` and asserts the collected 1xx
    /// list only where the count IS deterministic. Every absorbed provisional
    /// still feeds the dialog bookkeeping (early To-tag, Contact, route set),
    /// exactly as a sequence of `try_expect(18x)` calls would; the final
    /// overrides per §13.2.2.4. A non-matching FINAL is still a
    /// [`StepError::WrongStatus`].
    ///
    /// NOTE: under `--auto-retransmit` the mux absorbs a REPEATED
    /// byte-identical provisional as a retransmission before any body sees it
    /// — see the retransmit-engine notes in `loadgen::mux` (`CallTxns`); a
    /// "ring again" assertion belongs to the functional/e2e surface, not here.
    pub async fn try_expect_final(
        &mut self,
        status: u16,
    ) -> Result<(SipResponse, Vec<SipResponse>), StepError> {
        let mut provisionals = Vec::new();
        loop {
            match self.agent.try_recv().await? {
                SipMessage::Response(r) if r.status == 100 => continue,
                SipMessage::Response(r) if r.status < 200 => {
                    self.learn_from_response(&r);
                    provisionals.push(r);
                }
                SipMessage::Response(r) => {
                    // §17.1.1.3 txn-layer auto-ACK — matching or not, a non-2xx
                    // final to THIS INVITE completes its client transaction.
                    self.ack_ctx().ack_non_2xx(&r).await?;
                    if r.status != status {
                        return Err(StepError::WrongStatus {
                            who: self.agent.name.clone(),
                            expected: status,
                            got: r.status,
                            reason: r.reason.clone(),
                        });
                    }
                    self.learn_from_response(&r);
                    return Ok((r, provisionals));
                }
                SipMessage::Request(r) => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!("got a {} request, expected a {status} final", r.method),
                    });
                }
            }
        }
    }

    /// Receive the next response to this INVITE **without asserting its status**
    /// (an unsolicited `100 Trying` is still absorbed). Unlike
    /// [`try_expect`](Self::try_expect) this surfaces a `401`/`407` challenge as an
    /// `Ok(response)` the auth retry point can inspect, rather than collapsing it
    /// to `WrongStatus`. It does NOT learn dialog state (a non-2xx final seeds no
    /// dialog); the caller learns from a 2xx via [`try_expect`]. Like every
    /// receive on this transaction, a non-2xx final (a challenge, a shed 503,
    /// any reject) is auto-ACKed on arrival (§17.1.1.3) — the auth retry then
    /// resends under a FRESH branch, a new transaction.
    // Test-only scaffolding for the `ack_and_resend_with_auth` end-to-end unit
    // test: it reads a raw 401/407 with a BLOCKING receive. The production actor
    // caller never re-receives here — its reactor already surfaced the response
    // via `recv_any`, which it folds in with [`absorb_response`](Self::absorb_response)
    // (that path drives the live §22.2 retry). Hence `#[cfg(test)]`-only.
    #[cfg(test)]
    pub(crate) async fn try_recv_response(&mut self) -> Result<SipResponse, StepError> {
        loop {
            match self.agent.try_recv().await? {
                SipMessage::Response(r) if r.status == 100 => continue,
                SipMessage::Response(r) => {
                    self.ack_ctx().ack_non_2xx(&r).await?;
                    return Ok(r);
                }
                SipMessage::Request(r) => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!("got a {} request, expected a response", r.method),
                    });
                }
            }
        }
    }

    /// Fold an ALREADY-received response (surfaced via [`Agent::recv_any`]) into
    /// this INVITE transaction WITHOUT recv'ing again — the reactive-actor
    /// caller-side analogue of [`try_expect`](Self::try_expect), which owns its
    /// own receive. A provisional / 2xx updates the dialog bookkeeping (early
    /// tag, target, route set) exactly as `try_expect` would; a non-2xx final is
    /// auto-ACKed on its branch (§17.1.1.3), like every other receive on this
    /// transaction. On [`InviteResponseFate::Answered`] the caller then
    /// [`ack`](Self::ack)s to confirm the dialog. `100 Trying` is a provisional
    /// here (the reactor ignores it) — the actor never awaits a specific status.
    pub(crate) async fn absorb_response(
        &mut self,
        resp: &SipResponse,
    ) -> Result<InviteResponseFate, StepError> {
        if resp.status < 200 {
            self.learn_from_response(resp);
            return Ok(InviteResponseFate::Provisional { status: resp.status });
        }
        if (200..300).contains(&resp.status) {
            self.learn_from_response(resp);
            return Ok(InviteResponseFate::Answered);
        }
        // A non-2xx final completes the client transaction — auto-ACK it on the
        // INVITE branch (§17.1.1.3), matching the `try_expect` path.
        self.ack_ctx().ack_non_2xx(resp).await?;
        Ok(InviteResponseFate::Failed { status: resp.status })
    }

    /// Learn the remote tag / target / route set from a response — the dialog
    /// bookkeeping shared by [`expect`](Self::expect) and
    /// [`try_expect`](Self::try_expect).
    fn learn_from_response(&mut self, resp: &SipResponse) {
        // RFC 3261 §13.2.2.4 / §12.1: the 2xx to the INVITE establishes the
        // dialog, so its To-tag is THE confirmed remote tag — even when an
        // earlier provisional from a *different* fork (RFC 3261 §12.1.2) seeded
        // another. A provisional only seeds the (early) remote tag when none is
        // known yet; the final 2xx overrides it so the ACK and every subsequent
        // in-dialog request address the dialog the 2xx actually confirmed.
        let is_2xx_invite = (200..300).contains(&resp.status) && resp.cseq.method == "INVITE";
        if let Some(tag) = &resp.to.tag {
            if is_2xx_invite || self.dialog.remote_tag.is_empty() {
                self.dialog.remote_tag = tag.clone();
            }
        }
        if let Some(target) = first_contact_uri(resp) {
            self.dialog.remote_target = target;
        }
        // Build the dialog route set from the response's Record-Route, REVERSED
        // (UAC, RFC 3261 §12.1.2), once — so a later 200 doesn't re-seed it.
        if self.dialog.route_set.is_empty() {
            let rr = get_headers(&resp.headers, "record-route");
            if !rr.is_empty() {
                self.dialog.route_set = rr.iter().rev().map(|s| s.to_string()).collect();
            }
        }
    }

    /// The Request-URI this INVITE targets (its wire R-URI) — the request-line
    /// input a credential is computed over ([`ChallengeResponder::respond`]).
    pub fn ruri(&self) -> &str {
        &self.original_invite.uri
    }

    /// The establishing INVITE's CSeq number (the RETRIED INVITE's after a
    /// §22.2 auth resend re-pointed the transaction). A fork's 2xx echoes it —
    /// the fork-aware caller's discriminator for a LOSING fork's late 200
    /// (same CSeq as the INVITE, different To-tag than the winner's).
    pub fn invite_cseq(&self) -> u32 {
        self.original_invite.cseq.seq
    }

    /// The early dialog's learned remote (To) tag — from the first tagged
    /// provisional / the confirming 2xx (RFC 3261 §12.1.2). Empty before any
    /// tag is learned. The early UPDATE addresses this so it rides the early
    /// dialog's own CSeq sequence.
    pub fn early_remote_tag(&self) -> &str {
        &self.dialog.remote_tag
    }

    /// The confirmed [`Dialog`] a FORK's 2xx establishes — keyed by the
    /// response's OWN To-tag rather than the shared dialog state (which tracks
    /// the §13.2.2.4 winner). The losing-fork path: a late 2xx from a fork this
    /// INVITE created must be ACKed and BYE'd on ITS OWN dialog — its tag, its
    /// Contact, its route set (RFC 3261 §12.1.2). The CSeq continues from the
    /// fork's own PRACK sequence when it had one (`fork_cseq`), mirroring the
    /// winner promotion in [`ack`](Self::ack), so the fork's BYE stays
    /// contiguous within its dialog (§12.2.1.1).
    pub fn fork_dialog(&self, resp: &SipResponse) -> Dialog {
        let mut dialog = self.dialog.clone();
        if let Some(tag) = &resp.to.tag {
            dialog.remote_tag = tag.clone();
        }
        if let Some(target) = first_contact_uri(resp) {
            dialog.remote_target = target;
        }
        let rr = get_headers(&resp.headers, "record-route");
        if !rr.is_empty() {
            dialog.route_set = rr.iter().rev().map(|s| s.to_string()).collect();
        }
        if let Some(&fork) = self.fork_cseq.get(&dialog.remote_tag) {
            dialog.local_cseq = dialog.local_cseq.max(fork);
        }
        let dst = next_hop(&dialog, self.fallback_addr);
        Dialog::new(self.agent.clone(), dst, dialog)
    }

    /// **RFC 3261 §22.2 authentication retry** — the ONE auth adapter point wired
    /// into the INVITE choreography (see [`crate::realcall::auth`]). Given the
    /// `401`/`407` `challenge` this INVITE just drew and a configured `responder`:
    ///
    /// 1. ACK the challenge response (RFC 3261 §17.1.1.3: a non-2xx final to an
    ///    INVITE MUST be ACKed to complete the client transaction) via
    ///    `generate_ack_for_non_2xx`, echoing the INVITE's Via/Route;
    /// 2. ask the `responder` for the credential header value;
    /// 3. resend THIS INVITE with the credential header added, its CSeq bumped by
    ///    one, and a fresh Via branch (a new client transaction), and re-point
    ///    `self` (its `original_invite`/`cancel_handle`/`ack` targets) at the
    ///    retried transaction.
    ///
    /// Returns `Ok(true)` when it resent (the caller then awaits the retried
    /// transaction's response), `Ok(false)` when the responder DECLINED (no
    /// resend — the caller surfaces the original challenge as `status_401/407`),
    /// or `Err` on a transport failure. The default (no responder) never reaches
    /// here, so the classification is unchanged.
    pub(crate) async fn ack_and_resend_with_auth(
        &mut self,
        challenge: &SipResponse,
        responder: &dyn ChallengeResponder,
    ) -> Result<bool, StepError> {
        // 1. The challenged INVITE transaction is already complete: the receive
        //    that surfaced the challenge auto-ACKed it (§17.1.1.3).

        // 2. Ask the pluggable adapter for a credential. A missing/parsed-off
        //    challenge still fires the responder (a static fixture ignores it);
        //    `None` = decline → no retry.
        let parsed = parse_challenge(challenge).unwrap_or(crate::realcall::auth::Challenge {
            status: challenge.status,
            header_value: String::new(),
        });
        let method = self.original_invite.method.to_string();
        let Some(credential) = responder.respond(&parsed, &method, self.ruri()) else {
            return Ok(false);
        };

        // 3. Rebuild THIS INVITE as a new transaction: bump CSeq, fresh Via
        //    branch, add the credential header (RFC 3261 §22.2). Serialization is
        //    driven by the header list + first line + body, so rewriting the
        //    header vector (and re-parsing to keep the structured fields in step
        //    for the later ACK/CANCEL) is a faithful resend.
        let new_cseq = self.original_invite.cseq.seq + 1;
        let mut headers = self.original_invite.headers.clone();
        headers = set_header(&headers, "Via", &self.agent.via_header());
        headers = set_header(&headers, "CSeq", &format!("{new_cseq} {method}"));
        // Drop any prior credential of the same header (a second challenge round
        // would replace it) then add this one.
        headers.retain(|h| !h.name.eq_ignore_ascii_case(parsed.credential_header()));
        headers.push(SipHeader {
            name: parsed.credential_header().to_string(),
            value: credential,
        });

        let bytes = sip_message::serialize_request_parts(&self.original_invite, &headers);
        let resent = CustomParser::new().parse(&bytes).map_err(|e| StepError::Unparseable {
            who: self.agent.name.clone(),
            detail: format!("rebuilt authed INVITE did not parse: {e}"),
        })?;
        let SipMessage::Request(resent) = resent else {
            return Err(StepError::UnexpectedKind {
                who: self.agent.name.clone(),
                detail: "rebuilt authed INVITE parsed as a response".to_string(),
            });
        };

        self.agent.try_send(&SipMessage::Request(resent.clone()), self.wire_dst).await?;
        // Re-point the transaction state at the retried INVITE: the CANCEL / ACK /
        // dialog CSeq must all follow the new transaction, not the challenged one.
        self.original_invite = resent;
        self.dialog.local_cseq = new_cseq;
        Ok(true)
    }

    /// A cheap, `Send + 'static` handle that can CANCEL this still-pending INVITE
    /// later — the load driver registers it in its teardown scope so a call that
    /// fails *before* confirmation is CANCELled (RFC 3261 §9.1), never leaked on
    /// the SUT. Holds its own [`Agent`] clone (shared `Arc` endpoint), so it
    /// works even after the scenario's own handles are dropped.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            agent: self.agent.clone(),
            wire_dst: self.wire_dst,
            original_invite: self.original_invite.clone(),
        }
    }

    /// Send a CANCEL for this still-pending INVITE (RFC 3261 §9.1). The CANCEL
    /// reuses the INVITE's Request-URI / Call-ID / From / To / topmost Via branch
    /// and the INVITE's CSeq *number* with method `CANCEL`, and is sent to the
    /// SAME wire destination the INVITE took (the proxy / B2BUA when
    /// [`Invite::through`] was used). Returns a client transaction so the caller
    /// can `expect` the `200 OK` to the CANCEL; the matching `487 Request
    /// Terminated` for the INVITE arrives on this same UA and is consumed via
    /// [`ClientInvite::expect`].
    pub async fn cancel(&self) -> InDialogTxn {
        unwrap_step(try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await);
        InDialogTxn::new(
            self.agent.clone(),
            // A CANCEL transaction's finals take no ACK; the INVITE's 487 is
            // read — and auto-ACKed — via [`ClientInvite::expect`].
            None,
            self.wire_dst,
        )
    }

    /// Begin an in-dialog request on the *early* dialog (before the final 2xx /
    /// ACK) — the PRACK path (RFC 3262): alice PRACKs a reliable 183 while the
    /// INVITE transaction is still pending. The CSeq advances on the shared
    /// dialog state, so the later BYE numbers correctly.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        InDialogRequest::new(self.agent.clone(), &mut self.dialog, self.fallback_addr, method)
            .with_fork_cseq(&mut self.fork_cseq)
    }

    /// PRACK the reliable provisional `reliable_1xx` (RFC 3262 §7.2), fallibly:
    /// builds the `RAck` (`<RSeq> <CSeq-num> <CSeq-method>`) from the response's
    /// own RSeq + CSeq and sends the PRACK on the early dialog. Returns the PRACK
    /// client transaction to [`try_expect(200)`](InDialogTxn::try_expect) on. A
    /// response with no (or an unparseable) `RSeq` is not PRACK-able — that
    /// surfaces as [`StepError::UnexpectedKind`], never a panic.
    pub async fn try_prack(
        &mut self,
        reliable_1xx: &SipResponse,
    ) -> Result<InDialogTxn, StepError> {
        let rack = rack_for(reliable_1xx).ok_or_else(|| StepError::UnexpectedKind {
            who: self.agent.name.clone(),
            detail: format!(
                "cannot PRACK the {} {}: no parseable RSeq header (not a reliable provisional)",
                reliable_1xx.status, reliable_1xx.reason
            ),
        })?;
        self.send_request(InDialogMethod::Prack).with_rack(&rack).try_send().await
    }

    /// [`try_prack`](Self::try_prack) that also returns the PRACK request as
    /// sent — the reactive actor keys its "PRACK awaiting 200" ledger obligation
    /// on the returned request's CSeq (the 200 carries the same number). Same
    /// RAck derivation; the linear lane uses the request-less [`try_prack`].
    ///
    /// FORK-addressed: the PRACK belongs to the early dialog the reliable 1xx
    /// CREATED (RFC 3262 §5), so it is addressed under the response's own
    /// To-tag and rides that fork's independent CSeq sequence (`fork_cseq`,
    /// seeded from the INVITE's CSeq). For the unforked reliable answer this is
    /// byte-identical to the shared-counter path — the first PRACK is
    /// `INVITE_CSeq + 1` either way, and [`ack`](Self::ack) promotes the
    /// winning fork's sequence onto the confirmed dialog.
    pub async fn try_prack_with_request(
        &mut self,
        reliable_1xx: &SipResponse,
    ) -> Result<(InDialogTxn, SipRequest), StepError> {
        let rack = rack_for(reliable_1xx).ok_or_else(|| StepError::UnexpectedKind {
            who: self.agent.name.clone(),
            detail: format!(
                "cannot PRACK the {} {}: no parseable RSeq header (not a reliable provisional)",
                reliable_1xx.status, reliable_1xx.reason
            ),
        })?;
        let fork_tag = reliable_1xx.to.tag.clone();
        let mut req = self.send_request(InDialogMethod::Prack).with_rack(&rack);
        if let Some(tag) = &fork_tag {
            req = req.with_to_tag(tag);
        }
        req.try_send_with_request().await
    }

    /// Generate and send the ACK for the 2xx (CSeq reused from the INVITE per
    /// RFC 3261 §13.2.2.4), then return the confirmed [`Dialog`]. With a route
    /// set the ACK carries Route headers and goes to the first hop (the proxy).
    pub async fn ack(&mut self) -> Dialog {
        self.ack_with(None).await
    }

    /// ACK the 2xx carrying an optional SDP body — the delayed-offer answer
    /// rides the ACK when the 200 OK carried the offer (RFC 3264 §4).
    pub async fn ack_with(&mut self, sdp: Option<&str>) -> Dialog {
        let handle = InviteClientTransactionHandle {
            original_invite: self.original_invite.clone(),
        };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(Some(&handle), &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
        // If this dialog confirmed to a fork that carried its own PRACK sequence,
        // continue from THAT fork's CSeq so the post-confirm BYE/re-INVITE is
        // contiguous within the winning dialog (RFC 3261 §12.2.1.1), not a reuse
        // of a CSeq the fork already spent.
        let mut confirmed = self.dialog.clone();
        if let Some(&fork) = self.fork_cseq.get(&confirmed.remote_tag) {
            confirmed.local_cseq = confirmed.local_cseq.max(fork);
        }
        Dialog::new(self.agent.clone(), dst, confirmed)
    }
}

/// A `Send + 'static` CANCEL handle for a still-pending INVITE (see
/// [`ClientInvite::cancel_handle`]). The load driver's teardown scope holds one
/// for any call still in its early phase, so a failed call releases the SUT.
#[derive(Clone)]
pub struct CancelHandle {
    agent: Agent,
    wire_dst: SocketAddr,
    original_invite: SipRequest,
}

impl CancelHandle {
    /// Send a CANCEL for the pending INVITE (RFC 3261 §9.1) on a best-effort
    /// basis — a transport error is swallowed (the call is already failing). Does
    /// not wait for the 200/487.
    pub async fn cancel_best_effort(&self) {
        let _ = try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await;
    }
}
