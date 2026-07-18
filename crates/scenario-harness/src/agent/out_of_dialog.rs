//! [`OutOfDialogRequest`] — the any-method out-of-dialog builder (OPTIONS,
//! MESSAGE, REGISTER, …), including the RFC 3261 §22.2 authenticated send.
//! The dialog-creating INVITE has its own builder in [`super::client_invite`].

use std::net::SocketAddr;

use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::{EmitOpts, MessageTemplate, SipHeader, SipMessage, SipResponse};

use super::client_txn::recv_response_raw;
use super::dialog::InDialogTxn;
use super::step::{unwrap_step, StepError};
use super::Agent;
use crate::realcall::auth::{parse_challenge, ChallengeResponder};

/// Builder for a generic out-of-dialog request (any [`OutOfDialogMethod`]) —
/// see [`Agent::request`]. Mirrors [`Invite`](super::Invite)'s knobs (extra
/// headers, optional body, `through` wire routing, From/To/R-URI overrides)
/// with a **fallible** send for the load lane.
pub struct OutOfDialogRequest<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    method: OutOfDialogMethod,
    body: Option<Vec<u8>>,
    /// Content-Type for a non-empty body (defaults to `application/sdp`).
    content_type: Option<String>,
    extra_headers: Vec<SipHeader>,
    /// Wire destination override (send via a proxy/SUT; R-URI still targets peer).
    wire_dst: Option<SocketAddr>,
    from_uri: Option<String>,
    to_uri: Option<String>,
    request_uri: Option<String>,
}

impl<'a> OutOfDialogRequest<'a> {
    pub(super) fn new(caller: &'a Agent, peer: &'a Agent, method: OutOfDialogMethod) -> Self {
        OutOfDialogRequest {
            caller,
            peer,
            method,
            body: None,
            content_type: None,
            extra_headers: vec![],
            wire_dst: None,
            from_uri: None,
            to_uri: None,
            request_uri: None,
        }
    }

    /// Attach an SDP body (`Content-Type: application/sdp`).
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.body = Some(sdp.as_bytes().to_vec());
        self.content_type = None;
        self
    }

    /// Attach an arbitrary body with an explicit Content-Type.
    pub fn with_body(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self.content_type = Some(content_type.to_string());
        self
    }

    /// Attach an arbitrary extra header. Repeatable; order preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader { name: name.to_string(), value: value.to_string() });
        self
    }

    /// Emit this out-of-dialog request from a captured [`MessageTemplate`]: freeze
    /// the template's non-tier-1 headers (verbatim value/casing/duplicate layout)
    /// and its body, leaving the stack to regenerate the mechanical fields (Via +
    /// branch, Call-ID, From-tag, CSeq, Max-Forwards, Content-Length, Contact).
    /// The request method stays the one this builder was constructed with; the
    /// captured `Content-Type` rides as a frozen header. See [`EmitOpts`] for the
    /// v1 header-order limitation.
    pub fn template(mut self, tmpl: &MessageTemplate, opts: EmitOpts) -> Self {
        // v1: casing + duplicate-header layout are always preserved for frozen
        // headers; `preserve_order` requests nothing further yet (see EmitOpts).
        let EmitOpts { preserve_order: _ } = opts;
        self.extra_headers = tmpl.frozen_headers();
        self.body = Some(tmpl.body().to_vec());
        self.content_type = None;
        self
    }

    /// Send the request to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer) — the same wire routing as
    /// [`Invite::through`](super::Invite::through).
    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Override the From URI.
    pub fn from(mut self, uri: impl Into<String>) -> Self {
        self.from_uri = Some(uri.into());
        self
    }

    /// Override the To URI.
    pub fn to(mut self, uri: impl Into<String>) -> Self {
        self.to_uri = Some(uri.into());
        self
    }

    /// Override the Request-URI.
    pub fn ruri(mut self, uri: impl Into<String>) -> Self {
        self.request_uri = Some(uri.into());
        self
    }

    /// Generate the request (all mechanical headers filled in), send it
    /// **fallibly**, and return the client transaction to
    /// [`try_expect`](InDialogTxn::try_expect) the response on. A transport
    /// failure surfaces as [`StepError::Transport`], never a panic.
    pub async fn try_send(self) -> Result<InDialogTxn, StepError> {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let request_uri = self
            .request_uri
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let opts = GenerateOutOfDialogRequestOpts {
            request_uri,
            call_id: format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip()),
            from_uri: self.from_uri.unwrap_or_else(|| caller.uri.clone()),
            from_tag: caller.tag(),
            to_uri: self.to_uri.unwrap_or_else(|| peer.uri.clone()),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.body.unwrap_or_default(),
            content_type: self.content_type,
            extra_headers: self.extra_headers,
        };
        let req = generate_out_of_dialog_request(self.method, &opts);
        let msg = SipMessage::Request(req);
        caller.try_send(&msg, wire_dst).await?;
        let SipMessage::Request(request) = msg else { unreachable!() };
        Ok(InDialogTxn::new(
            caller.clone(),
            // An out-of-dialog INVITE's non-2xx final takes a txn-layer ACK
            // (§17.1.1.3) — retain the request so the txn can build it.
            matches!(self.method, OutOfDialogMethod::Invite).then_some(request),
            wire_dst,
        ))
    }

    /// Panicking [`try_send`](Self::try_send) for functional tests.
    pub async fn send(self) -> InDialogTxn {
        unwrap_step(self.try_send().await)
    }

    /// **RFC 3261 §22.2 authenticated send** — the out-of-dialog twin of the
    /// INVITE choreography's auth retry (see [`crate::realcall::auth`]), for a
    /// REGISTER / OPTIONS shape against a challenging registrar. Sends the
    /// request, awaits its `expect` final; if it is a `401`/`407` and `responder`
    /// is `Some`, adds the credential (a non-INVITE final needs no ACK,
    /// RFC 3261 §17.1.2.2), bumps the CSeq, and resends ONCE with a fresh Via
    /// branch, then awaits again. `responder == None` (the default) makes this a
    /// plain send-and-await with no retry — a `401`/`407` surfaces as
    /// `WrongStatus`, exactly as `try_send` + `try_expect` would.
    pub async fn try_send_authed(
        self,
        responder: Option<&dyn ChallengeResponder>,
        expect: u16,
    ) -> Result<SipResponse, StepError> {
        let caller = self.caller.clone();
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let request_uri = self
            .request_uri
            .clone()
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let mut opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip()),
            from_uri: self.from_uri.clone().unwrap_or_else(|| caller.uri.clone()),
            from_tag: caller.tag(),
            to_uri: self.to_uri.clone().unwrap_or_else(|| peer.uri.clone()),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.body.clone().unwrap_or_default(),
            content_type: self.content_type.clone(),
            extra_headers: self.extra_headers.clone(),
        };
        let method = self.method;

        // At most ONE authenticated resend.
        let mut auth_retries_left: u8 = if responder.is_some() { 1 } else { 0 };
        loop {
            let req = generate_out_of_dialog_request(method, &opts);
            caller.try_send(&SipMessage::Request(req), wire_dst).await?;
            // Raw-receive so a 401/407 keeps its challenge header (a real digest
            // responder reads `nonce`/`realm` off it); a matching final returns
            // straight away, an unsolicited 100 is absorbed.
            let resp = recv_response_raw(&caller).await?;
            if resp.status == expect {
                return Ok(resp);
            }
            let is_challenge = matches!(resp.status, 401 | 407);
            // Not a retriable challenge (or no retry budget): surface the
            // deviation exactly as `try_expect(expect)` would.
            if !(is_challenge && auth_retries_left > 0 && responder.is_some()) {
                return Err(StepError::WrongStatus {
                    who: caller.name.clone(),
                    expected: expect,
                    got: resp.status,
                    reason: resp.reason.clone(),
                });
            }
            let responder = responder.expect("guarded above");
            let challenge = parse_challenge(&resp).unwrap_or(crate::realcall::auth::Challenge {
                status: resp.status,
                header_value: String::new(),
            });
            // Responder declines → surface the challenge as a plain deviation.
            let Some(credential) =
                responder.respond(&challenge, method.as_str(), &request_uri)
            else {
                return Err(StepError::WrongStatus {
                    who: caller.name.clone(),
                    expected: expect,
                    got: resp.status,
                    reason: resp.reason.clone(),
                });
            };
            // A non-INVITE final needs no ACK (§17.1.2.2). Resend with the
            // credential, a bumped CSeq, and a fresh Via branch (a new
            // transaction, §22.2).
            opts.cseq += 1;
            opts.via = Some(caller.via());
            opts.extra_headers
                .retain(|h| !h.name.eq_ignore_ascii_case(challenge.credential_header()));
            opts.extra_headers.push(SipHeader {
                name: challenge.credential_header().to_string(),
                value: credential,
            });
            auth_retries_left -= 1;
        }
    }
}
