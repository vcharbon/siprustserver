//! [`Invite`] — the initial (dialog-creating) INVITE builder. Its `send`
//! returns the UAC-side [`ClientInvite`](super::ClientInvite) transaction,
//! which lives in [`super::client_invite`].

use std::collections::HashMap;
use std::net::SocketAddr;

use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
    StackDialog,
};
use sip_message::{apply_name_forms, EmitOpts, MessageTemplate, SipHeader, SipMessage};

use super::client_invite::ClientInvite;
use super::Agent;

/// Builder for an outgoing INVITE (lets the SDP offer be attached fluently).
pub struct Invite<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    sdp: Option<String>,
    /// Raw body bytes from a [`MessageTemplate`] (overrides `sdp` when set) — the
    /// captured payload emitted verbatim, its Content-Type carried as a frozen
    /// header rather than stamped by the generator.
    template_body: Option<Vec<u8>>,
    /// A template body carried NO Content-Type: suppress the generator's default
    /// `application/sdp` stamp so a captured bodyless-typed / non-SDP payload
    /// replays with the Content-Type the capture actually had (none).
    suppress_default_ct: bool,
    /// `(canonical, wire)` name-forms for the stack-regenerated headers the
    /// capture wrote compact (`("Via","v")`, …) — applied to the WIRE copy only.
    name_forms: Vec<(String, String)>,
    extra_headers: Vec<SipHeader>,
    /// Wire destination override — the INVITE is *addressed* to `peer` (its
    /// Contact is the Request-URI) but *sent* here. Set by [`Invite::through`]
    /// to route an initial INVITE via a proxy/LB.
    wire_dst: Option<SocketAddr>,
    /// Optional From/To/Request-URI overrides — the seam an E2E *Test case*
    /// uses to drive From/To/R-URI from input data (numbers) instead of the
    /// default `sip:name@ip` agent identities. `None` keeps the default.
    from_uri: Option<String>,
    to_uri: Option<String>,
    request_uri: Option<String>,
}

impl<'a> Invite<'a> {
    pub(super) fn new(caller: &'a Agent, peer: &'a Agent) -> Self {
        Invite {
            caller,
            peer,
            sdp: None,
            template_body: None,
            suppress_default_ct: false,
            name_forms: vec![],
            extra_headers: vec![],
            wire_dst: None,
            from_uri: None,
            to_uri: None,
            request_uri: None,
        }
    }

    /// Attach an SDP offer body.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Emit this INVITE from a captured [`MessageTemplate`]: the stack
    /// regenerates the tier-1 dialog fields (Via + branch, Call-ID, From/To tags,
    /// CSeq, Max-Forwards, Content-Length, Contact, Request-URI) while every
    /// frozen header rides verbatim (value bytes, name casing, duplicate-header
    /// layout). The captured body is emitted as-is; its `Content-Type`, being a
    /// frozen header, is carried rather than stamped by the generator.
    ///
    /// The template's start line must be a request of method `INVITE`. See
    /// [`EmitOpts`] for the v1 header-order limitation. Automatic/dedicated-
    /// primitive messages (`ACK`/`CANCEL`) take no template in v1 (see
    /// [`MessageTemplate`]).
    pub fn template(mut self, tmpl: &MessageTemplate, opts: EmitOpts) -> Self {
        // v1: casing + duplicate-header layout are always preserved for frozen
        // headers; `preserve_order` requests nothing further yet (see EmitOpts).
        let EmitOpts { preserve_order: _ } = opts;
        assert!(
            matches!(tmpl.method(), Some(sip_message::Method::Invite)),
            "Invite::template requires an INVITE request template, got {:?}",
            tmpl.start()
        );
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
        // Compact wire name-forms for the stack-regenerated headers (Via/From/…).
        self.name_forms = tmpl.regenerated_name_forms();
        self
    }

    /// Attach an arbitrary extra header on the initial INVITE (e.g. `Supported:
    /// 100rel, timer` to drive the 18x-management strategies).
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Override the From URI (e.g. `"sip:+33123456789@example.com"`) — drives
    /// From from Test-case input instead of the default `sip:caller@ip`.
    pub fn from(mut self, uri: impl Into<String>) -> Self {
        self.from_uri = Some(uri.into());
        self
    }

    /// Override the To URI — drives To from Test-case input. The To URI also
    /// seeds the dialog's remote URI.
    pub fn to(mut self, uri: impl Into<String>) -> Self {
        self.to_uri = Some(uri.into());
        self
    }

    /// Override the Request-URI — drives the R-URI from Test-case input. The
    /// INVITE is still *sent* to the peer/`through` wire destination.
    pub fn ruri(mut self, uri: impl Into<String>) -> Self {
        self.request_uri = Some(uri.into());
        self
    }

    /// Send the initial INVITE to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer). Used to drive an LB/record-routing
    /// proxy; subsequent in-dialog requests then follow the route set learned
    /// from the proxy's Record-Route automatically.
    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Generate the INVITE (all headers filled in), send it, and return the
    /// client transaction handle.
    pub async fn send(self) -> ClientInvite {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let call_id = format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip());
        let from_tag = caller.tag();
        // Default identities are the agent URIs / a peer-addressed R-URI; a Test
        // case may override any of From/To/R-URI from its input data.
        let request_uri = self
            .request_uri
            .clone()
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let from_uri = self.from_uri.clone().unwrap_or_else(|| caller.uri.clone());
        let to_uri = self.to_uri.clone().unwrap_or_else(|| peer.uri.clone());

        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: call_id.clone(),
            from_uri: from_uri.clone(),
            from_tag: from_tag.clone(),
            to_uri: to_uri.clone(),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            // A captured template body (raw bytes, any Content-Type) overrides
            // the SDP-string offer; its Content-Type rides as a frozen header.
            body: self
                .template_body
                .clone()
                .or_else(|| self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec))
                .unwrap_or_default(),
            content_type: None,
            extra_headers: self.extra_headers.clone(),
        };
        let mut invite = generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
        if self.suppress_default_ct {
            invite.headers =
                sip_message::message_helpers::remove_header(&invite.headers, "content-type");
        }
        // Send a WIRE copy with the captured compact names on the tier-1 lines;
        // `invite` keeps canonical names so the §17.1.1.3 ACK / §9.1 CANCEL
        // header lookups (get_header, not compact-aware) still resolve.
        let mut wire = invite.clone();
        wire.headers = apply_name_forms(&invite.headers, &self.name_forms);
        caller.send(&SipMessage::Request(wire), wire_dst).await;

        let dialog = StackDialog {
            call_id,
            local_tag: from_tag,
            remote_tag: String::new(),
            local_uri: from_uri,
            remote_uri: to_uri,
            remote_target: request_uri,
            local_cseq: 1,
            route_set: vec![],
        };
        ClientInvite {
            agent: caller.clone(),
            fallback_addr: peer.addr,
            wire_dst,
            original_invite: invite,
            dialog,
            fork_cseq: HashMap::new(),
        }
    }
}
