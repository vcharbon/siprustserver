//! The [`Respond`] builder of a [`ServerTxn`]: To-tag minting, SDP answer,
//! reliable 18x, the Contact it states, Record-Route echo folding,
//! §17.1.1.3 ACK-obligation arming.

use sip_message::generators::{
    generate_response, GenerateResponseOpts, B2BUA_ACCEPT, B2BUA_ALLOW, B2BUA_SUPPORTED,
};
use sip_message::header::{self, HeaderName, MediaType};
use sip_message::{SipHeader, SipMessage};
use sip_template::{apply_name_forms, apply_remote_target_emits, emitted_wire};

use crate::{EmitOpts, MessageTemplate};

use super::super::addressing::{top_via_addr, top_via_branch};
use super::super::rr_fold::{fold_record_routes, RecordRouteFold};
use super::super::step::{unwrap_step, StepError};
use super::super::ua::{media_type, uri_of};
use super::{ServerTxn, DECLARE_WINNER};

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
    /// The media type [`with_body`](Self::with_body) states for its bytes.
    content_type: Option<MediaType>,
    /// A template body carried NO Content-Type: suppress the generator's default
    /// `application/sdp` stamp (see [`Invite::template`](super::Invite::template)).
    suppress_default_ct: bool,
    /// `(canonical, wire)` name-forms for stack-regenerated headers the capture
    /// wrote compact (see [`Invite::template`](super::Invite::template)).
    name_forms: Vec<(String, String)>,
    /// `(canonical, captured_value)` remote-target headers (Contact) — captured
    /// user + params over the bound socket's host:port.
    remote_emits: Vec<(String, String)>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
    /// The Contact URI this response states; `None` names the UA's socket.
    contact: Option<String>,
}

impl<'a> Respond<'a> {
    pub(super) fn new(txn: &'a mut ServerTxn, status: u16, reason: &str) -> Self {
        Respond {
            txn,
            status,
            reason: reason.to_string(),
            sdp: None,
            template_body: None,
            content_type: None,
            suppress_default_ct: false,
            name_forms: vec![],
            remote_emits: vec![],
            extra_headers: vec![],
            to_tag: None,
            contact: None,
        }
    }

    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Attach a body of any media type, byte-exact — a `multipart/mixed`
    /// answer framing SDP beside another part, a non-SDP payload.
    pub fn with_body(mut self, content_type: &str, bytes: Vec<u8>) -> Self {
        self.template_body = Some(bytes);
        self.content_type = Some(media_type(content_type));
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
        // A replay emits the header block it captured: a template that states a
        // media type in ANY spelling keeps that line and the stack adds none, and
        // one that states none must not gain the stack's default.
        self.suppress_default_ct = !frozen.iter().any(|h| HeaderName::ContentType.matches(&h.name));
        // Append AFTER any prior `with_header` entries — never drop them.
        self.extra_headers.extend(frozen);
        self.template_body = Some(tmpl.body().to_vec());
        self.name_forms = tmpl.regenerated_name_forms();
        self.remote_emits = tmpl.remote_target_emits();
        self
    }

    /// Force a specific To-tag on this response instead of the auto-minted one.
    /// Used to simulate a forking endpoint emitting several early dialogs with
    /// distinct tags (RFC 3261 §12.1; the per-fork 18x in `prack-forking`).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// State `uri` as this response's Contact, the dialog's remote target at
    /// the peer (RFC 3261 §12.1.2): a user part holding `;` or escapes
    /// (§25.1), or a host that is no socket of this UA's.
    pub fn with_contact(mut self, uri: &str) -> Self {
        self.contact = Some(uri.to_string());
        self
    }

    /// Attach a custom header (e.g. `Require: 100rel`, `RSeq: 1` on a reliable
    /// provisional, RFC 3262). Repeatable; order is preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers
            .push(SipHeader { name: name.to_string().into(), value: value.to_string().into() });
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
        // Silent-misuse guards the wire audit cannot catch (all wire-legal): a
        // response after the transaction's final, and a forked 2xx with no winner
        // declared. A same-status retransmission of the final is legal
        // (RFC 3261 §13.3.1.4) and passes through untouched.
        if let Some(fin) = txn.final_status {
            assert!(
                self.status == fin,
                "respond {} on an INVITE server transaction already finalized with {fin} (RFC 3261 §17.2.1): the transaction is complete",
                self.status
            );
        } else {
            assert!(
                !((200..300).contains(&self.status)
                    && !txn.early_tags.is_empty()
                    && txn.winner.is_none()),
                "{} early dialog(s) open and no winner declared before the 2xx — {DECLARE_WINNER}",
                txn.early_tags.len()
            );
            if self.status >= 200 {
                txn.final_status = Some(self.status);
            }
        }
        // An explicit per-fork To-tag overrides (and does not disturb) the txn's
        // sticky tag, the first fork's if any (RFC 3261 §8.2.6.2): each early dialog keeps its own.
        let to_tag = if let Some(t) = self.to_tag {
            Some(t)
        } else {
            if self.status > 100 && txn.to_tag.is_none() {
                txn.to_tag = txn.provisional_tag.clone().or_else(|| Some(txn.agent.tag()));
            }
            txn.to_tag.clone()
        };
        if txn.request.to().tag().is_none() {
            match self.status {
                101..=199 if txn.provisional_tag.is_none() => txn.provisional_tag = to_tag.clone(),
                200..=299 => txn.dialog_tag = to_tag.clone(),
                _ => {}
            }
        }
        // Contact is required on 2xx and useful on 18x to establish the early
        // dialog's remote target; omit on plain 100.
        let contact = match (&self.contact, self.status >= 180) {
            (_, false) => None,
            (Some(uri), true) => Some(header::Contact::from_uri(uri_of(uri))),
            (None, true) => Some(txn.agent.contact()),
        };
        // A conformant UAS lists its methods/extensions on a 2xx INVITE (RFC 3261
        // §13.2.1 SHOULD Allow, §20.37 Supported) and its capabilities on a 2xx
        // OPTIONS (§11.2). The test UA answers anything, so it adds each one the
        // fixture did not supply — a frozen compact `k:` counts as Supported
        // (§7.3.3) — matching the live SIPp endpoints.
        let mut extra_headers = self.extra_headers.clone();
        let method = txn.request.cseq().method().as_str();
        let advertises =
            (200..300).contains(&self.status) && matches!(method, "INVITE" | "OPTIONS");
        for (name, value, on) in [
            (HeaderName::Allow, B2BUA_ALLOW, advertises),
            (HeaderName::Supported, B2BUA_SUPPORTED, advertises),
            (HeaderName::Accept, B2BUA_ACCEPT, advertises && method == "OPTIONS"),
        ] {
            if on && !extra_headers.iter().any(|h| name.matches(&h.name)) {
                extra_headers
                    .push(SipHeader { name: name.as_wire_str().into(), value: value.into() });
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
            content_type: self.content_type.clone(),
            extra_headers,
            incoming_source: None,
        };
        let mut resp = generate_response(&txn.request, self.status, &self.reason, &opts).unwrap();
        if self.suppress_default_ct {
            resp = resp
                .thaw()
                .remove(&HeaderName::ContentType)
                .freeze()
                .expect("dropping the stack's media type leaves a complete response");
        }
        // Real UAs may fold multiple echoed Record-Route rows into one comma-
        // separated header (RFC 3261 §7.3.1); reproduce that wire form for UAs the
        // harness picked `Combined` for, so the B2BUA's split-before-§12.1.2-reverse
        // path is exercised on the b-leg route-set capture (see `RecordRouteFold`).
        let mut headers = resp.headers().to_vec();
        if txn.agent.rr_fold == RecordRouteFold::Combined {
            fold_record_routes(&mut headers);
        }
        // Emit stack-regenerated headers under the captured compact names (Via/
        // From/To/…) when the template used them; identity for a full-name capture.
        if !self.name_forms.is_empty() {
            headers = apply_name_forms(&headers, &self.name_forms);
        }
        if !self.remote_emits.is_empty() {
            headers = apply_remote_target_emits(&headers, &self.remote_emits);
        }
        // Responses are routed by Via, not Route (RFC 3261 §18.2.2): send to the
        // request's topmost Via sent-by. With a proxy in the path that Via is
        // the proxy's, so the response correctly traverses it back.
        let dst = top_via_addr(&txn.request).unwrap_or(txn.agent.addr);
        let msg = SipMessage::Response(resp);
        txn.agent.try_send_emitted(&msg, &emitted_wire(&msg, &headers), dst).await?;
        // §17.1.1.3 UAS obligation: a non-2xx final to an INVITE (initial or
        // re-INVITE) arms the txn-owned ACK wait — the arriving hop ACK is the
        // transaction layer's to claim, in whatever order it lands relative to
        // the body's next receive; `expect_ack` asserts it and the gating
        // `unacked-invite-non-2xx-final` wire rule settles it at finish.
        if (300..700).contains(&self.status) && txn.request.method().as_str() == "INVITE" {
            if let Some(branch) = top_via_branch(&txn.request) {
                txn.agent.txn.arm_hop_ack(txn.request.call_id().to_string(), branch);
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
