//! The UAS side: [`ServerTxn`] (a received request + its response state) and
//! the [`Respond`] builder (To-tag minting, SDP answer, reliable 18x,
//! Record-Route echo folding, §17.1.1.3 ACK-obligation arming).

use sip_message::generators::{
    generate_response, GenerateResponseOpts, StackDialog, B2BUA_ALLOW, B2BUA_SUPPORTED,
};
use sip_message::message_helpers::{extract_contact_uri, get_header, get_headers};
use sip_message::{
    apply_name_forms, EmitOpts, MatchOpts, MessageTemplate, Mismatch, SipHeader, SipMessage,
    SipRequest,
};

use super::addressing::{next_hop, top_via_addr, top_via_branch};
use super::dialog::Dialog;
use super::rr_fold::{fold_record_routes, RecordRouteFold};
use super::step::{unwrap_step, StepError};
use super::Agent;

/// UAS-side transaction for a received request. `respond` echoes Via/From/To/
/// Call-ID/CSeq and mints a stable To-tag on the first non-100 response.
pub struct ServerTxn {
    agent: Agent,
    pub(super) request: SipRequest,
    to_tag: Option<String>,
    route_set: Vec<String>,
}

impl ServerTxn {
    /// The ONE constructor from a received request. Captures the UAS route set
    /// (RFC 3261 §12.1.1): the request's Record-Route rows in received order —
    /// used if this UAS later originates in-dialog requests (e.g. bob sends
    /// the BYE).
    pub(super) fn from_request(agent: Agent, request: SipRequest) -> Self {
        let route_set = get_headers(&request.headers, "record-route")
            .iter()
            .map(|s| s.to_string())
            .collect();
        ServerTxn { agent, request, to_tag: None, route_set }
    }

    /// The received request (for inspecting headers / SDP).
    pub fn request(&self) -> &SipRequest {
        &self.request
    }

    /// Assert this received request matches `tmpl` under the tiered match model
    /// ([`MessageTemplate::match_inbound`]): tier-1 fields structural-only,
    /// remote-target headers compared by params modulo `opts.ignore_params`,
    /// frozen headers + body byte-compared. Returns the first [`Mismatch`].
    /// The response-side equivalent is `tmpl.match_inbound(&SipMessage::Response(resp), opts)`
    /// on the response a client transaction's `expect` returned.
    pub fn expect_template(
        &self,
        tmpl: &MessageTemplate,
        opts: &MatchOpts,
    ) -> Result<(), Mismatch> {
        tmpl.match_inbound(&SipMessage::Request(self.request.clone()), opts)
    }

    /// Send a response. Returns a builder for attaching an SDP answer and/or
    /// custom headers (e.g. `Require: 100rel` + `RSeq` on a reliable 18x).
    pub fn respond(&mut self, status: u16, reason: &str) -> Respond<'_> {
        Respond {
            txn: self,
            status,
            reason: reason.to_string(),
            sdp: None,
            template_body: None,
            suppress_default_ct: false,
            name_forms: vec![],
            extra_headers: vec![],
            to_tag: None,
        }
    }

    /// Answer this request from a captured [`MessageTemplate`]: the status /
    /// reason come from the template's response start line; the stack regenerates
    /// the echoed tier-1 fields (Via, From, To + minted tag, Call-ID, CSeq,
    /// Contact, Content-Length) while every frozen header rides verbatim (value
    /// bytes, name casing, duplicate-header layout). The captured `Content-Type`
    /// is carried as a frozen header. Panics if the template is not a response.
    /// See [`EmitOpts`] for the v1 header-order limitation.
    pub fn respond_template<'t>(
        &'t mut self,
        tmpl: &MessageTemplate,
        opts: EmitOpts,
    ) -> Respond<'t> {
        let (status, reason) = tmpl
            .status()
            .unwrap_or_else(|| panic!("respond_template requires a response template, got {:?}", tmpl.start()));
        let reason = reason.to_string();
        self.respond(status, &reason).template(tmpl, opts)
    }

    /// Assert the §17.1.1.3 hop ACK for THIS transaction's non-2xx final.
    /// Returns immediately if the transaction layer already claimed it (it may
    /// have landed before or after any of the body's other receives — matching
    /// is by `(Call-ID, INVITE branch)`, never positional); otherwise pulls
    /// until the ACK arrives, the panicking-veneer sibling of
    /// [`try_expect_ack`](Self::try_expect_ack). Purely optional: an unread ACK
    /// is still recorded at delivery and the gating
    /// `unackedInviteNon2xxFinal` wire rule settles the obligation at
    /// `finish()` — reach for this when the test asserts the ACK at a specific
    /// point in the flow.
    pub async fn expect_ack(&self) {
        unwrap_step(self.try_expect_ack().await)
    }

    /// Fallible core of [`expect_ack`](Self::expect_ack).
    pub async fn try_expect_ack(&self) -> Result<(), StepError> {
        let Some(branch) = top_via_branch(&self.request.headers) else {
            return Err(StepError::UnexpectedKind {
                who: self.agent.name.clone(),
                detail: "expect_ack on a request with no top-Via branch".to_string(),
            });
        };
        loop {
            if self.agent.acks.is_fulfilled(&self.request.call_id, &branch) {
                return Ok(());
            }
            // Pull; the receive core sights (and thereby fulfils) a matching
            // ACK. Anything else arriving while the test explicitly awaits the
            // ACK is a deviation, exactly as `receive("ACK")` would treat it.
            match self.agent.try_recv().await? {
                SipMessage::Request(r) if r.method.as_str() == "ACK" => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // ours (loop returns Ok) or another txn's obligation
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: "got an ACK for a different transaction".to_string(),
                    });
                }
                SipMessage::Request(r) => {
                    return Err(StepError::WrongMethod {
                        who: self.agent.name.clone(),
                        expected: "ACK".to_string(),
                        got: r.method.to_string(),
                    })
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected the ACK to this txn's final",
                            r.status, r.reason
                        ),
                    })
                }
            }
        }
    }

    /// Pin this transaction's sticky local To-tag (the dialog-forming tag) to an
    /// explicit value — the forking-UAS winner path: after emitting per-fork 18x
    /// with explicit tags (`Respond::with_to_tag`, which deliberately does NOT
    /// disturb the sticky tag), the winning fork's 200 must both carry the
    /// winner's tag AND leave [`dialog`](Self::dialog) keyed under it. Call
    /// before responding the winning 2xx.
    pub fn adopt_to_tag(&mut self, tag: &str) {
        self.to_tag = Some(tag.to_string());
    }

    /// Form the UAS-side confirmed [`Dialog`] for this transaction, so this UA
    /// can originate in-dialog requests (e.g. the callee sends the BYE). Call
    /// after responding 2xx (so the To-tag is minted). The remote target is the
    /// caller's Contact; the route set is the request's Record-Route in order
    /// (§12.1.1), so in-dialog requests route back through any proxy.
    pub fn dialog(&self) -> Dialog {
        let req = &self.request;
        let local_tag = self.to_tag.clone().unwrap_or_default();
        let remote_target = get_header(&req.headers, "contact")
            .map(extract_contact_uri)
            .unwrap_or_else(|| req.from.uri.clone());
        let dialog = StackDialog {
            call_id: req.call_id.clone(),
            local_tag,
            remote_tag: req.from.tag.clone().unwrap_or_default(),
            // From the UAS's view, "local" is itself and "remote" is the caller.
            // RFC 3261 §12.1.1: the dialog LOCAL URI is the To field of the
            // request, NOT the agent's own AOR — they coincide in the usual
            // alice→bob case (To:bob == bob's uri) but diverge when a UAS is
            // handed an INVITE addressed to a third party (e.g. the MRF media
            // leg carries To:dest): the callee's in-dialog requests must then
            // carry From:dest, and the recorded-trace midDialogUri audit — which
            // merges both tag orientations into one dialog slice — checks it.
            local_uri: req.to.uri.clone(),
            remote_uri: req.from.uri.clone(),
            remote_target,
            local_cseq: 0, // UAS originates its own CSeq space; first request → 1
            route_set: self.route_set.clone(),
        };
        let fallback = next_hop(&dialog, top_via_addr(req).unwrap_or(self.agent.addr));
        Dialog::new(self.agent.clone(), fallback, dialog)
    }
}

/// Builder for a UAS response (lets an SDP answer and custom headers be
/// attached fluently).
pub struct Respond<'a> {
    txn: &'a mut ServerTxn,
    status: u16,
    reason: String,
    sdp: Option<String>,
    /// Raw body bytes from a [`MessageTemplate`] (overrides `sdp` when set) — the
    /// captured payload emitted verbatim, its Content-Type carried as a frozen
    /// header rather than stamped by the generator.
    template_body: Option<Vec<u8>>,
    /// A template body carried NO Content-Type: suppress the generator's default
    /// `application/sdp` stamp (see [`Invite::template`](super::Invite::template)).
    suppress_default_ct: bool,
    /// `(canonical, wire)` name-forms for stack-regenerated headers the capture
    /// wrote compact (see [`Invite::template`](super::Invite::template)).
    name_forms: Vec<(String, String)>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
}

impl<'a> Respond<'a> {
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Freeze a captured [`MessageTemplate`]'s non-tier-1 headers (verbatim
    /// value/casing/duplicate layout) and its body onto this response; the stack
    /// still echoes/regenerates Via/From/To(+tag)/Call-ID/CSeq/Contact/
    /// Content-Length. Status and reason stay as given to [`ServerTxn::respond`]
    /// (see [`ServerTxn::respond_template`] to derive them from the template).
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
        self.template_body = Some(tmpl.body().to_vec());
        self.name_forms = tmpl.regenerated_name_forms();
        self
    }

    /// Force a specific To-tag on this response instead of the auto-minted one.
    /// Used to simulate a forking endpoint emitting several early dialogs with
    /// distinct tags (RFC 3261 §12.1; the per-fork 18x in `prack-forking`).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach a custom header (e.g. `Require: 100rel`, `RSeq: 1` on a reliable
    /// provisional, RFC 3262). Repeatable; order is preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Mark this provisional RELIABLE (RFC 3262 §3): stamps `Require: 100rel` +
    /// `RSeq: <rseq>` so the peer must PRACK it. Only meaningful on a 101–199
    /// response to a dialog-creating INVITE whose sender opted in
    /// (`Supported`/`Require: 100rel`).
    pub fn reliable(self, rseq: u32) -> Self {
        self.with_header("Require", "100rel").with_header("RSeq", &rseq.to_string())
    }

    /// Generate and send the response. Panics on a transport failure — use
    /// [`try_send`](Self::try_send) in the load lane.
    pub async fn send(self) {
        unwrap_step(self.try_send().await)
    }

    /// Fallible [`send`](Self::send) for the load lane: a transport failure
    /// surfaces as [`StepError::Transport`] instead of a panic.
    pub async fn try_send(self) -> Result<(), StepError> {
        let txn = self.txn;
        // An explicit per-fork To-tag overrides (and does not disturb) the txn's
        // sticky auto-minted tag, so distinct early dialogs keep distinct tags.
        let to_tag = if let Some(t) = self.to_tag {
            Some(t)
        } else {
            if self.status > 100 && txn.to_tag.is_none() {
                txn.to_tag = Some(txn.agent.tag());
            }
            txn.to_tag.clone()
        };
        // Contact is required on 2xx and useful on 18x to establish the early
        // dialog's remote target; omit on plain 100.
        let contact = if self.status >= 180 {
            Some(txn.agent.contact())
        } else {
            None
        };
        // A conformant UAS lists its methods/extensions on a 2xx INVITE
        // (RFC 3261 §13.2.1 SHOULD Allow, §20.37 Supported) so the peer can
        // negotiate re-INVITE/UPDATE/PRACK. The test UA answers anything, so
        // add them (unless the fixture already supplied one) — the UA stays
        // RFC-compliant, matching the live SIPp endpoints.
        let mut extra_headers = self.extra_headers.clone();
        if (200..300).contains(&self.status) && txn.request.cseq.method.as_str() == "INVITE" {
            // Compact-aware probes: a frozen `k:` (compact Supported) must
            // suppress the stack default, else the replayed 2xx advertises
            // 100rel/timer the capture never did (RFC 3261 §7.3.3).
            let has_allow = extra_headers
                .iter()
                .any(|h| sip_message::message_helpers::name_matches("Allow", &h.name));
            let has_supported = extra_headers
                .iter()
                .any(|h| sip_message::message_helpers::name_matches("Supported", &h.name));
            if !has_allow {
                extra_headers.push(SipHeader { name: "Allow".into(), value: B2BUA_ALLOW.into() });
            }
            if !has_supported {
                extra_headers
                    .push(SipHeader { name: "Supported".into(), value: B2BUA_SUPPORTED.into() });
            }
        }
        let opts = GenerateResponseOpts {
            to_tag,
            contact,
            // A captured template body (raw bytes, any Content-Type) overrides
            // the SDP-string answer; its Content-Type rides as a frozen header.
            body: self
                .template_body
                .clone()
                .or_else(|| self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec))
                .unwrap_or_default(),
            content_type: None,
            extra_headers,
            incoming_source: None,
        };
        let mut resp = generate_response(&txn.request, self.status, &self.reason, &opts);
        if self.suppress_default_ct {
            resp.headers =
                sip_message::message_helpers::remove_header(&resp.headers, "content-type");
        }
        // Real UAs may fold multiple echoed Record-Route rows into one comma-
        // separated header (RFC 3261 §7.3.1); reproduce that wire form for UAs the
        // harness picked `Combined` for, so the B2BUA's split-before-§12.1.2-reverse
        // path is exercised on the b-leg route-set capture (see `RecordRouteFold`).
        if txn.agent.rr_fold == RecordRouteFold::Combined {
            fold_record_routes(&mut resp.headers);
        }
        // Emit stack-regenerated headers under the captured compact names (Via/
        // From/To/…) when the template used them; identity for a full-name capture.
        if !self.name_forms.is_empty() {
            resp.headers = apply_name_forms(&resp.headers, &self.name_forms);
        }
        // Responses are routed by Via, not Route (RFC 3261 §18.2.2): send to the
        // request's topmost Via sent-by. With a proxy in the path that Via is
        // the proxy's, so the response correctly traverses it back.
        let dst = top_via_addr(&txn.request).unwrap_or(txn.agent.addr);
        txn.agent.try_send(&SipMessage::Response(resp), dst).await?;
        // §17.1.1.3 UAS obligation: a non-2xx final to an INVITE (initial or
        // re-INVITE) arms the txn-owned ACK wait — the arriving hop ACK is the
        // transaction layer's to claim, in whatever order it lands relative to
        // the body's next receive; `expect_ack` asserts it and the gating
        // `unackedInviteNon2xxFinal` wire rule settles it at finish.
        if (300..700).contains(&self.status) && txn.request.method.as_str() == "INVITE" {
            if let Some(branch) = top_via_branch(&txn.request.headers) {
                txn.agent.acks.arm(txn.request.call_id.clone(), branch);
            }
        }
        Ok(())
    }
}

/// Allow `respond(...).await` directly (no explicit `.send()`), by making the
/// builder awaitable.
impl<'a> std::future::IntoFuture for Respond<'a> {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}
