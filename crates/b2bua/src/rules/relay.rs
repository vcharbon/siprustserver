//! Relay primitives. Unlike a transparent proxy, the B2BUA *regenerates*
//! messages on the peer leg's own transaction/dialog (back-to-back UAs): a
//! response from bob is rebuilt as a fresh response on alice's INVITE server
//! transaction (stable a-facing To-tag), and an in-dialog request is rebuilt on
//! the peer dialog. This keeps the two dialogs independent (their tags, CSeq
//! spaces and Contacts are the B2BUA's own), which is the whole point of a
//! B2BUA. Source: `ActionExecutor.ts` relay paths + `b2bua/helpers.ts`.

use call::{
    B2buaDialogExt, Dialog, InviteTxnHandle, Leg, LegDisposition, LegState, RemoteInfo, StackDialog,
};
use sip_message::draft::{Entry, RequestDraft};
use sip_message::generators::{
    self, CapabilitySet, GenerateAckFor2xxOpts, GenerateOutOfDialogRequestOpts,
    GenerateResponseOpts, OutOfDialogMethod, RelayScope,
};
use sip_message::header::{
    self, ChargingVector, HeaderName, HeaderValue, HostPort, MaxForwards, MediaType, NameAddr,
    ParamValue, RouteEntry, Uri, Via,
};
use sip_message::{Method, SipHeader as MsgHeader, SipRequest, SipStr};
use sip_txn::{IdGen, TxnKind};

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::stack_identity::{build_call_contact, build_call_via, StackIdentityOpts};

/// An address a decision named that no reader accepts, so the B2BUA has nothing
/// to route toward. `Display` names the offending **field only** — it is what a
/// refusal puts in a reason phrase, and the field is this stack's own static
/// text, so nothing a peer wrote can reach the wire through it. The value and
/// the reader's reason ride [`detail`](Self::detail), for the local log.
///
/// The B2BUA never answers such a field by inventing a value (upstreamneed-055):
/// an opaque URI's host is the whole raw text, so originating on one dials an
/// address nobody named — the failure rides this error to a seam that can 5xx
/// the affected leg and write the CDR.
#[derive(Debug, Clone)]
pub struct UnreadableAddress {
    /// The decision field the text came from (`new_ruri`, `contact`, …).
    pub field: &'static str,
    /// The text as the decision stated it.
    pub value: String,
    /// Why no reader accepts it.
    pub reason: String,
}

impl std::fmt::Display for UnreadableAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unreadable Routing Address ({})", self.field)
    }
}

impl std::error::Error for UnreadableAddress {}

impl UnreadableAddress {
    /// The full detail, value included — for a local log, never for the wire.
    pub fn detail(&self) -> String {
        format!("{}={:?}: {}", self.field, self.value, self.reason)
    }
}

/// The address `text` names, or a refusal. The one reader every decision-
/// supplied address on the routing path goes through.
fn address(field: &'static str, text: &str) -> Result<Uri, UnreadableAddress> {
    Uri::parse(&SipStr::owned(text)).map_err(|err| UnreadableAddress {
        field,
        value: text.to_string(),
        reason: err.reason,
    })
}

/// The media type a policy- or peer-supplied value names. The B2BUA emits its
/// own body, so the header describing it is the stack's to state (§16.6). Text
/// the reader rejects is carried as the peer wrote it rather than replaced by a
/// media type this stack invented — the body is still the peer's.
pub fn media_type(text: &str) -> Option<MediaType> {
    Some(MediaType::parse(&SipStr::owned(text)).unwrap_or_else(|_| MediaType::new(SipStr::owned(text))))
}

/// `application/sdp` — the media type the B2BUA's own offers and answers carry.
pub fn sdp() -> MediaType {
    MediaType::new(SipStr::from_static("application/sdp"))
}

/// The transparency scope of the INVITE this B2BUA originates: a decision that
/// replaces the originator's body (a held REFER offer, or none) takes the
/// headers describing that body with it.
fn relay_scope(body_override: Option<&[u8]>) -> RelayScope {
    let scope = RelayScope::request();
    match body_override {
        Some(_) => scope.without_source_body(),
        None => scope,
    }
}

/// One `Contact: <uri>;q=…` redirect target (RFC 3261 §20.10) for a 3xx the
/// B2BUA authors. A target no reader accepts is refused: the caller dials what
/// a 3xx Contact names, so an invented one sends it at an address the decision
/// never stated.
pub fn redirect_contact(uri: &str, q: Option<f32>) -> Result<MsgHeader, UnreadableAddress> {
    let mut contact = header::Contact::new(NameAddr::new(address("contact", uri)?));
    if let Some(q) = q {
        contact = contact.with_param("q", ParamValue::text(SipStr::owned(&q.to_string())));
    }
    Ok(MsgHeader {
        name: SipStr::owned(HeaderName::Contact.as_wire_str()),
        value: SipStr::owned(&contact.to_wire()),
    })
}

/// Convert a `call` dialog to the generators' `StackDialog` input shape.
pub fn to_gen_dialog(d: &StackDialog) -> generators::StackDialog {
    generators::StackDialog {
        call_id: d.call_id.clone(),
        local_tag: d.local_tag.clone(),
        remote_tag: d.remote_tag.clone(),
        local_uri: d.local_uri.clone(),
        remote_uri: d.remote_uri.clone(),
        remote_target: d.remote_target.clone(),
        local_cseq: d.local_cseq.max(0) as u32,
        route_set: d.route_set.clone(),
    }
}

/// Rebuild the a-leg's original INVITE as a `SipRequest` (for `generate_response`).
/// Every header rides as an unparsed line, so the rebuilt message carries the
/// caller's bytes exactly as they arrived. A caller that omitted `Max-Forwards`
/// gets RFC 3261 §8.1.1.6's default — the rebuild is a request the type system
/// holds, and nothing reads the hop count off it.
///
/// The Request-URI is read verbatim, not refused: this is a round-trip of text
/// the inbound parser's own strict gates already admitted, so a refusal here
/// would drop a call the stack accepted. Nothing routes on it — the b-leg's
/// Request-URI comes from the decision, through [`build_b_leg`]'s reader.
pub fn rebuild_a_leg_invite(snap: &call::ALegInviteSnapshot) -> SipRequest {
    let mut draft = RequestDraft::new(Method::Invite, Uri::parse_or_verbatim(&SipStr::owned(&snap.uri)));
    for h in &snap.headers {
        draft = draft.push_raw(HeaderName::from(h.name.as_str()), SipStr::owned(&h.value));
    }
    if !draft.has(&HeaderName::MaxForwards) {
        draft = draft.push(MaxForwards::new(70));
    }
    draft
        .with_body(snap.body.clone().into())
        .freeze()
        .expect("a-leg INVITE snapshot is well-formed")
}

/// The B2BUA's Via for a leg's outbound message. `is_emergency` is the call's
/// emergency state (`call.emergency == Some(true)`); when set it stamps the
/// `;em=1` marker every subsequent in-dialog packet of the call then carries,
/// so an admitted emergency call stays identifiable on the wire.
pub fn leg_via(
    config: &B2buaConfig,
    call_ref: &str,
    leg_id: &str,
    is_emergency: bool,
    branch: String,
) -> Via {
    build_call_via(
        &StackIdentityOpts {
            local_ip: &config.sip_local_ip,
            local_port: config.sip_local_port,
            call_ref,
            leg: leg_id,
            is_emergency,
        },
        branch,
    )
}

/// The B2BUA's Contact for a leg's outbound message. `is_emergency` (the call's
/// `call.emergency == Some(true)`) stamps the `;emerg=1` Contact marker — see
/// [`leg_via`].
pub fn leg_contact(
    config: &B2buaConfig,
    call_ref: &str,
    leg_id: &str,
    is_emergency: bool,
) -> header::Contact {
    build_call_contact(&StackIdentityOpts {
        local_ip: &config.sip_local_ip,
        local_port: config.sip_local_port,
        call_ref,
        leg: leg_id,
        is_emergency,
    })
}

/// The transport destination the address `target` names. The `call` crate keeps
/// dialog targets and route sets as text (it has no sip-message dependency by
/// design, ADR-0008), so reading one back is a parse; an address no reader
/// accepts falls back to its own text on the default port, and says so — the
/// fallback resolves a host nothing routes to, so it must not pass unnamed.
pub fn target_dest(target: &str) -> (String, u16) {
    match NameAddr::parse(&SipStr::owned(target)) {
        Ok(addr) => {
            let (host, port) = addr.uri().host_port();
            (host.to_string(), port)
        }
        Err(err) => {
            if !target.trim().is_empty() {
                eprintln!("WARN: dialog target {target:?} does not read ({err}); resolving it as a host name");
            }
            (target.trim().to_string(), HostPort::DEFAULT_PORT)
        }
    }
}

/// Apply the egress routing policy to an outbound in-dialog request (port of
/// `ActionExecutor.ts` `applyEgressRouting` / `applyRouteSet`).
///
/// Two effects, both RFC 3261 §16.12:
///   1. **Loose-route wire destination** (any leg): when the dialog's route set
///      is non-empty and its first route is a loose router (`;lr`), the request
///      is *sent* to that route's host:port while the Request-URI stays at the
///      remote target. The generator already emitted the Route headers from the
///      route set; this fixes only the wire destination so in-dialog requests
///      toward a record-routing proxy traverse it instead of going pod-direct.
///   2. **b-leg outbound-proxy bootstrap**: when the route set is empty (the
///      pre-confirmation initial INVITE), `leg_id` is a b-leg, and
///      `config.b2b_outbound_proxy` is set, preload a *plain* loose `Route` at the
///      proxy and redirect the wire destination there (the b-leg invariant: every
///      B2BUA→callee message traverses the front proxy). The proxy classifies the
///      initial INVITE worker-outbound from the top Via and double-record-routes
///      the dialog, so in-dialog direction is carried by the proxy's own
///      Record-Route thereafter — the worker stamps no `;outbound` (`ProxyCore`
///      §16.4 / §16.12).
///
/// `route_set` is the source dialog's route set in dialog order. For the a-leg
/// the natural route set (from the inbound INVITE's Record-Route) carries the
/// routing; `b2b_outbound_proxy` is a b-leg concept and is not applied there.
pub fn apply_b_leg_egress(
    config: &B2buaConfig,
    leg_id: &str,
    route_set: &[String],
    req: SipRequest,
    dest: (String, u16),
) -> (SipRequest, (String, u16)) {
    // (1) Loose-route: send to the top route's host:port (R-URI unchanged).
    if let Some(first) = route_set.first() {
        if let Some(uri) = loose_route_uri(first) {
            // The worker no longer stamps `;outbound`: the front proxy double-
            // record-routes, so the worker-facing half of the dialog route set —
            // captured from the dialog-creating message (§12.1.1/§12.1.2) — is
            // ALREADY the proxy's own `;outbound` Record-Route on top. The proxy
            // reads direction from its own self-issued RR (registry- and pod-IP-
            // independent, so it survives a worker reboot), not from anything the
            // worker adds. We just forward the route set verbatim to the proxy.
            let (host, port) = uri.host_port();
            return (req, (host.to_string(), port));
        }
        // Strict routing is handled by the generator's R-URI rewrite; the wire
        // destination already resolves to the first route via `remote_target`.
        return (req, dest);
    }
    // (2) Empty route set (pre-confirmation INVITE) + b-leg outbound-proxy
    // bootstrap. There is no dialog route set yet, so preload a plain loose Route
    // to the front proxy to get the initial INVITE there; the proxy classifies it
    // worker-outbound from the top Via (the originating worker is live and
    // registered at call set-up — the reboot window only affects in-dialog traffic
    // of EXISTING calls, which the double-record-route above covers) and double-
    // record-routes the dialog so every subsequent in-dialog request is direction-
    // correct without a worker-stamped marker.
    if leg_id == "a" {
        return (req, dest);
    }
    let Some((route, (host, port))) = outbound_proxy_route(config) else {
        return (req, dest);
    };
    match req.thaw().prepend(route).freeze() {
        Ok(preloaded) => (preloaded, (host, port)),
        // The preload failed, so the request carries no Route naming the proxy —
        // but its Request-URI already names the callee, so the proxy forwards it
        // and record-routes the dialog. Sending it to `dest` instead would put
        // this leg pod-direct, which the deployment forbids (every worker-
        // originated request traverses the front proxy).
        Err(err) => {
            eprintln!(
                "WARN: leg {leg_id}: b-leg egress could not preload the outbound-proxy Route \
                 ({err}); forwarding to {host}:{port} WITHOUT it rather than pod-direct"
            );
            (req, (host, port))
        }
    }
}

/// The plain loose `Route` naming the configured front proxy, with the wire
/// destination it resolves to. `None` when no outbound proxy is configured
/// (local/dev, where the transport IS peer-direct).
fn outbound_proxy_route(config: &B2buaConfig) -> Option<(RouteEntry, (String, u16))> {
    let (host, port) = config.b2b_outbound_proxy.clone()?;
    let route = RouteEntry::from_uri(Uri::sip(host.clone()).with_port(port).with_flag("lr"));
    Some((route, (host, port)))
}

/// The dialog route set a call falls back to when the peer's recorded routes do
/// not read: the one loose `Route` at the configured front proxy — the same
/// entry [`apply_b_leg_egress`] preloads for a pre-confirmation b-leg — so
/// in-dialog requests keep traversing the proxy instead of going pod-direct.
/// Empty when no outbound proxy is configured.
pub fn outbound_proxy_route_set(config: &B2buaConfig) -> Vec<String> {
    outbound_proxy_route(config)
        .map(|(route, _)| route.to_wire())
        .into_iter()
        .collect()
}

/// The URI of `route` when it names a loose router (RFC 3261 §19.1.1 `;lr`).
fn loose_route_uri(route: &str) -> Option<Uri> {
    let entry = RouteEntry::parse(&SipStr::owned(route)).ok()?;
    entry.uri().is_loose_route().then(|| entry.uri().clone())
}

/// Egress-aware wire destination for a leg's in-dialog request, WITHOUT mutating
/// a request. Mirrors `apply_b_leg_egress`'s destination decision (keep in sync) —
/// used for observability attribution (the keepalive-timeout peer metric), where
/// we need the wire hop the unanswered OPTIONS used but must not synthesize a
/// request to find it.
pub fn leg_egress_dest(
    config: &B2buaConfig,
    leg_id: &str,
    route_set: &[String],
    base_dest: (String, u16),
) -> (String, u16) {
    if let Some(first) = route_set.first() {
        if let Some(uri) = loose_route_uri(first) {
            let (host, port) = uri.host_port();
            return (host.to_string(), port);
        }
        return base_dest;
    }
    if leg_id == "a" {
        return base_dest;
    }
    if let Some((host, port)) = config.b2b_outbound_proxy.clone() {
        return (host, port);
    }
    base_dest
}

/// Build a fresh b-leg + its outbound INVITE effect (initial route + failover).
///
/// Errs when a decision-supplied address (`new_ruri` / `new_from` / `new_to`)
/// does not read: there is no b-leg to originate, and the caller answers the
/// affected leg instead of dialing a fabricated target (upstreamneed-055).
#[allow(clippy::too_many_arguments)]
pub fn build_b_leg(
    call_ref: &str,
    leg_id: &str,
    // The call's emergency state (`call.emergency == Some(true)`); stamps the
    // `;em=1` / `;emerg=1` markers on the originated b-leg INVITE's Via +
    // Contact, so every subsequent in-dialog packet of the call stays
    // identifiable as emergency traffic on the wire.
    is_emergency: bool,
    a_leg_invite: &SipRequest,
    dest: (String, u16),
    new_ruri: Option<&str>,
    // Identity rewrites (ADR-0017): override the b-leg From/To **URI** (the
    // from/to numbers). The B2BUA always owns the tags, so only the URI is
    // settable here; `None` keeps the relayed a-leg URI. The basic path passes
    // `(None, None)`.
    new_from: Option<&str>,
    new_to: Option<&str>,
    no_answer_timeout_sec: Option<i64>,
    config: &B2buaConfig,
    id_gen: &IdGen,
    // REFER transfer overrides: `body_override` replaces the cloned a-leg body
    // (held SDP, or empty = drop); `header_updates` set/remove extra headers on
    // the C INVITE. The basic-B2BUA path passes `(None, &[])`.
    body_override: Option<&[u8]>,
    header_updates: &[(String, Option<String>)],
    // Capability set advertised on this originated leg (`Allow`/`Supported`),
    // resolved by the caller: declared, else relayed from the originator, else
    // the stack set (`rules::capabilities`). A `header_updates` entry naming
    // either header is more specific and wins.
    capabilities: &CapabilitySet,
    // RFC 7315 §5.6 charging correlation. `Some` stamps an icid identifying
    // this leg's charging session; `None` stamps none. A vector the originator
    // sent is relayed either way and never re-minted — re-minting it breaks the
    // correlation between the two operators' records.
    charging: Option<&call::features::ChargingVectorFeature>,
    // Leg role (ADR-0014/0016). `None` ⇒ [`LegKind::Destination`]. `adopted` is
    // left `None` so it derives from the kind (`is_adopted`): a `media` leg is
    // unadopted and thus gated out of the generic relay-to-peer fallback.
    kind: Option<call::LegKind>,
) -> Result<(Leg, OutboundSipEffect), UnreadableAddress> {
    let branch = id_gen.new_branch();
    let from_tag = id_gen.new_tag();
    let b_call_id = format!("{}-{}@{}", leg_id, id_gen.new_tag(), config.sip_local_ip);
    // Each rewrite is read here or the leg is not built. `None` keeps the
    // relayed a-leg value, which the parser already accepted.
    let request_uri = match new_ruri {
        Some(text) => address("new_ruri", text)?,
        None => a_leg_invite.request_uri().clone(),
    };
    let from_uri = match new_from {
        Some(text) => address("new_from", text)?,
        None => a_leg_invite.from().uri().clone(),
    };
    let to_uri = match new_to {
        Some(text) => address("new_to", text)?,
        None => a_leg_invite.to().uri().clone(),
    };
    let body = match body_override {
        Some(b) => b.to_vec(),
        None => a_leg_invite.body().to_vec(),
    };
    let content_type = if body.is_empty() {
        None
    } else {
        a_leg_invite
            .raw(HeaderName::ContentType)
            .next()
            .and_then(media_type)
            .or_else(|| body_override.map(|_| sdp()))
    };
    // `(name, Some(v))` sets, `(name, None)` removes. Removals never apply to
    // structural headers (the generator owns those); only extra sets ride here.
    let mut extra_headers: Vec<MsgHeader> = header_updates
        .iter()
        .filter_map(|(n, v)| {
            v.as_ref().map(|val| MsgHeader { name: n.clone().into(), value: val.clone().into() })
        })
        .collect();
    // Advertise accepted methods + understood extensions on the originated b-leg
    // INVITE so the callee can negotiate UPDATE/PRACK/etc. (RFC 3261 §20.5/§20.37,
    // RFC 3311 §5) and the 2xx/re-INVITE audit (§13.2.1) sees a capability set.
    // `Supported` is a *default*: when a `relayFirst18x` strategy is active,
    // `apply_supported_for_18x` runs after this and rewrites it from alice's value
    // (stripping `100rel` as the strategy dictates). Neither clobbers a
    // caller-supplied value from `header_updates`.
    for (name, value) in [
        (HeaderName::Allow, capabilities.allow_text()),
        (HeaderName::Supported, capabilities.supported_text()),
    ] {
        if !extra_headers.iter().any(|h| name.matches(&h.name)) {
            extra_headers.push(MsgHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&value),
            });
        }
    }

    // Names the deployment states must ride, copied from the a-leg INVITE. A
    // name the relay withholds is refused here too, so configuration cannot
    // reach past the transparency rules; a value `header_updates` already set
    // stands (case-insensitive).
    for configured in &config.relay_headers {
        let name = HeaderName::from(configured.as_str());
        if extra_headers.iter().any(|h| name.matches(&h.name))
            || !generators::relayable(configured, relay_scope(body_override))
        {
            continue;
        }
        if let Some(v) = a_leg_invite.raw_text(name.clone()).next() {
            extra_headers.push(MsgHeader { name: SipStr::owned(configured), value: v });
        }
    }

    // RFC 3261 §16.6: every header the originator sent that this stack does not
    // own rides onto the leg it originates — one mint point for BOTH originated
    // legs (the callee leg, and the REFER transfer leg whose `a_leg_invite` is
    // alice's rehydrated snapshot). A name stated above is the more specific
    // statement and stands: an explicit `header_updates` value, then this face's
    // advertisement, then the relayed value.
    let stated = extra_headers.clone();
    for header in generators::relayable_headers(a_leg_invite.headers(), relay_scope(body_override))
    {
        let name = HeaderName::from(header.name.as_str());
        if !stated.iter().any(|h| name.matches(&h.name)) {
            extra_headers.push(header);
        }
    }

    // RFC 7315 §5.6: the element that STARTS a leg generates the identifier its
    // charging session is correlated on. One already on the message — relayed
    // from the originator, or stated by the decision — is that identifier, so
    // this only ever mints where none arrived.
    if let Some(charging) = charging {
        let name = ChargingVector::header_name();
        if !extra_headers.iter().any(|h| name.matches(&h.name)) {
            let host =
                charging.generated_at.clone().unwrap_or_else(|| config.sip_local_ip.clone());
            let icid = format!("{}-{}", id_gen.new_tag(), leg_id);
            extra_headers.push(MsgHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&ChargingVector::new(icid, host).to_wire()),
            });
        }
    }

    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(request_uri.clone()),
        call_id: b_call_id.clone(),
        from: Some(header::From::from_uri(from_uri.clone()).with_tag(SipStr::owned(&from_tag))),
        to: Some(header::To::from_uri(to_uri.clone())),
        cseq: 1,
        via: Some(leg_via(config, call_ref, leg_id, is_emergency, branch.clone())),
        contact: Some(leg_contact(config, call_ref, leg_id, is_emergency)),
        max_forwards: Some(70),
        body,
        content_type,
        extra_headers,
    };
    let invite = generators::generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
    // Behind the front proxy, the b-leg INVITE traverses the proxy: preload a
    // plain loose Route and make the wire destination the proxy (R-URI stays the
    // callee). The proxy classifies the initial INVITE worker-outbound from the
    // top Via and double-record-routes the dialog. `wire_dest` drives the client
    // transaction's send + retransmits; the preloaded Route rides the snapshot so
    // retransmits carry it too.
    let (invite, wire_dest) = apply_b_leg_egress(config, leg_id, &[], invite, dest.clone());

    // The `call` crate stores dialog identity as text (ADR-0008), so the values
    // this INVITE was built from are written down as the bytes it carries.
    let from_uri = from_uri.text().into_owned();
    let to_uri = to_uri.text().into_owned();
    let request_uri = request_uri.text().into_owned();

    let dialog = Dialog {
        sip: StackDialog {
            call_id: b_call_id.clone(),
            local_tag: from_tag.clone(),
            remote_tag: String::new(),
            local_uri: from_uri.clone(),
            remote_uri: to_uri.clone(),
            remote_target: request_uri.clone(),
            local_cseq: 1,
            route_set: vec![],
        },
        ext: B2buaDialogExt {
            remote_cseq: None,
            inbound_pending_requests: vec![],
            ack_branch: None,
            pending_invite_txn: Some(InviteTxnHandle {
                branch: branch.clone(),
                original_invite: invite.image().to_vec(),
                destination: call::HostPort {
                    host: wire_dest.0.clone(),
                    port: wire_dest.1,
                },
            }),
            cached_sdp: None,
            pending_reinvite_2xx: None,
        },
    };

    // Capture the INVITE handle before `dialog` is moved into the leg.
    let leg_invite_handle = dialog.ext.pending_invite_txn.clone();
    let leg = Leg {
        leg_id: leg_id.to_string(),
        call_id: b_call_id,
        from_tag,
        source: RemoteInfo {
            address: dest.0.clone(),
            port: dest.1,
        },
        state: LegState::Trying,
        disposition: LegDisposition::Pending,
        dialogs: vec![dialog],
        no_answer_timeout_sec,
        bye_disposition: None,
        local_uri: Some(from_uri),
        remote_uri: Some(to_uri),
        invite_request_uri: Some(request_uri),
        // Also stamp the INVITE handle on the leg: a forked early dialog created
        // from a later 18x has no per-dialog handle, so ACK-for-2xx / RAck CSeq
        // fall back to the leg's (RFC 3261 §13.2.2.4 / RFC 3262 §7.2).
        pending_invite_txn: leg_invite_handle,
        ext: None,
        kind: Some(kind.unwrap_or(call::LegKind::Destination)),
        // Derive adoption from the kind (don't pin it): Destination ⇒ adopted,
        // Media ⇒ unadopted. See `call::helpers::is_adopted`.
        adopted: None,
    };

    let effect = OutboundSipEffect {
        body: OutboundBody::Request(invite),
        mode: OutboundTxnMode::NewClient(TxnKind::Invite),
        destination: wire_dest,
        label: format!("b-leg INVITE ({leg_id})"),
        leg_id: Some(leg_id.to_string()),
    };
    Ok((leg, effect))
}

/// What the B2BUA carries transparently from a b-leg response onto the response
/// it mints toward the a-leg (RFC 3261 §16.6): every header it does not own,
/// which includes the reliable-provisional negotiation end to end
/// (`Require`/`Supported`/`RSeq`, RFC 3262). `keeps_body` states whether the
/// relayed response carries this response's own body — a policy that drops or
/// replaces the body leaves the headers describing it behind.
///
/// This is plain transparent relay — distinct from the B2BUA-side 18x
/// management *policies* (`relayFirst18xTo180`/`promote18xPemTo200`), which
/// *rewrite* these provisionals, and from the a-facing 2xx advertisement
/// [`stamp_a_facing_invite_advert`] owns.
pub fn relay_response_passthrough_headers(
    resp: &sip_message::SipResponse,
    keeps_body: bool,
) -> Vec<MsgHeader> {
    let scope = RelayScope::response();
    let scope = if keeps_body { scope } else { scope.without_source_body() };
    generators::relayable_headers(resp.headers(), scope)
}

/// Ensure an a-facing INVITE 2xx header set carries exactly ONE `Allow` and ONE
/// `Supported` — the capability set advertised toward the originator (RFC 3261
/// §13.2.1/§20.37). `capabilities` is the set for this face, which the caller
/// resolves from the declaration, the callee's own relayed advertisement and
/// the stack set (`rules::capabilities::relaying`); `rule_stamped` names the
/// headers the firing rule already set with its own value — those are more
/// specific, so they are kept verbatim and only de-duplicated. For the rest the
/// passed-through lines collapse into the single resolved value. Either way
/// exactly one of each results (no §7.3.1 duplicate); `Require`/`RSeq`
/// (reliable-provisional negotiation) are untouched.
pub fn stamp_a_facing_invite_advert(
    headers: &mut Vec<MsgHeader>,
    rule_stamped: &[Entry],
    capabilities: &CapabilitySet,
) {
    for (name, value) in [
        (HeaderName::Allow, capabilities.allow_text()),
        (HeaderName::Supported, capabilities.supported_text()),
    ] {
        if rule_stamped.iter().any(|e| e.is(&name)) {
            // The rule owns this value; just collapse any duplicate to one.
            let mut seen = false;
            headers.retain(|h| {
                if name.matches(&h.name) {
                    let keep = !seen;
                    seen = true;
                    keep
                } else {
                    true
                }
            });
            continue;
        }
        // Replace any passed-through value with this face's set, exactly once.
        headers.retain(|h| !name.matches(&h.name));
        headers.push(MsgHeader {
            name: SipStr::owned(name.as_wire_str()),
            value: SipStr::owned(&value),
        });
    }
}

/// What the B2BUA carries transparently when relaying an in-dialog *request*
/// across the back-to-back UA: every header of the received request it does not
/// own (RFC 3261 §16.6). A relayed REFER keeps its `Refer-To`/`Referred-By`
/// (without them it is malformed) and a relayed NOTIFY its
/// `Event`/`Subscription-State` — the generator states those from its own opts
/// ONLY for a B2BUA-originated NOTIFY, which the relay path leaves unset, so
/// nothing duplicates. The generator restates `RAck` per RFC 3262 §7.2, so the
/// received one is withheld rather than copied.
///
/// `target_declared` names the advertisement halves the face this request is
/// relayed toward states for itself. The peer's value for those is NOT copied:
/// it is a relayed value, not an explicit instruction, and copying it would
/// silently revert the declared narrowing on every re-INVITE.
pub fn relay_request_passthrough_headers(
    req: &SipRequest,
    target_declared: &[HeaderName],
) -> Vec<MsgHeader> {
    let mut headers = generators::relayable_headers(req.headers(), RelayScope::request());
    headers.retain(|h| !target_declared.iter().any(|name| name.matches(&h.name)));
    headers
}

/// Build a UAS response on a leg's inbound INVITE (toward alice). `to_tag` pins
/// the stable a-facing dialog tag.
#[allow(clippy::too_many_arguments)]
pub fn response_to_a_leg(
    a_leg_invite: &SipRequest,
    status: u16,
    reason: &str,
    to_tag: Option<String>,
    contact: Option<header::Contact>,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    incoming_source: Option<(String, u16)>,
    extra_headers: Vec<MsgHeader>,
) -> OutboundSipEffect {
    let opts = GenerateResponseOpts {
        to_tag,
        contact,
        body,
        content_type,
        extra_headers,
        incoming_source,
    };
    let resp = generators::generate_response(a_leg_invite, status, reason, &opts);
    // Routed by the txn layer to the a-leg server transaction; dest is alice
    // (top Via sent-by of her INVITE, RFC 3261 §18.2.2).
    let hop = a_leg_invite.top_via();
    let (host, port) = hop.sent_by().pair();
    let dest = (host.to_string(), port);
    OutboundSipEffect {
        body: OutboundBody::Response(resp),
        mode: OutboundTxnMode::ServerResponse,
        destination: dest,
        label: format!("{status} → a-leg"),
        leg_id: Some("a".to_string()),
    }
}

/// Build an ACK-for-2xx on a b-leg dialog (toward bob), sent raw. `body` carries
/// the inbound ACK's payload through (the delayed-offer re-INVITE answer rides
/// the ACK, RFC 3264 §4); pass empty for a bodyless ACK.
///
/// Returns the effect **and the Via branch it used**, so the caller can retain
/// the branch on the dialog ([`call::helpers::retain_ack_branch`]) for a
/// §13.2.2.4 re-ACK of a retransmitted 2xx.
pub fn ack_b_leg(
    call_ref: &str,
    leg: &Leg,
    is_emergency: bool,
    config: &B2buaConfig,
    id_gen: &IdGen,
    body: Vec<u8>,
    content_type: Option<MediaType>,
) -> Option<(OutboundSipEffect, String)> {
    let dialog = leg.dialogs.first()?;
    let gen_dialog = to_gen_dialog(&dialog.sip);
    // RFC 3261 §13.2.2.4: the ACK for a 2xx is a UAC-core retransmit target. The
    // answerer re-sends its 2xx end-to-end until ACKed (up to its Timer H ≈ 32 s),
    // so a retransmitted 2xx MUST be re-ACKed reusing the SAME Via branch — a
    // fresh branch would mint a *new* client transaction and never quiesce the
    // answerer's INVITE server txn, leaking / late-timing-out the confirmed call
    // when the first ACK is lost. Reuse the branch retained from the first ACK on
    // this INVITE transaction's 2xx; mint one on the first ACK. `ack_branch` is
    // reset wherever a new INVITE transaction is cached on the dialog, so a
    // `Some(_)` here always belongs to the CSeq echoed just below.
    let branch = dialog
        .ext
        .ack_branch
        .clone()
        .unwrap_or_else(|| id_gen.new_branch());
    // The ACK reuses the CSeq of the INVITE it acknowledges — not the dialog's
    // running `local_cseq`, which an intervening early PRACK/UPDATE (or a later
    // in-dialog request) has advanced past the INVITE. Recover it from the cached
    // INVITE transaction handle.
    let ack_cseq = acked_invite_cseq(dialog).unwrap_or_else(|| dialog.sip.local_cseq.max(0) as u32);
    let opts = GenerateAckFor2xxOpts {
        via: Some(leg_via(config, call_ref, &leg.leg_id, is_emergency, branch.clone())),
        cseq: Some(ack_cseq),
        body,
        content_type,
        ..Default::default()
    };
    let ack = generators::generate_ack_for_2xx(None, &gen_dialog, &opts);
    let dest = target_dest(&dialog.sip.remote_target);
    let (ack, dest) = apply_b_leg_egress(config, &leg.leg_id, &gen_dialog.route_set, ack, dest);
    Some((
        OutboundSipEffect {
            body: OutboundBody::Request(ack),
            mode: OutboundTxnMode::Raw,
            destination: dest,
            label: format!("ACK → {}", leg.leg_id),
            leg_id: Some(leg.leg_id.clone()),
        },
        branch,
    ))
}

/// The CSeq sequence number of the INVITE last sent on this dialog (initial or
/// re-INVITE), recovered from the cached client-transaction handle so the
/// 2xx ACK can echo it (RFC 3261 §13.2.2.4).
pub(crate) fn acked_invite_cseq(dialog: &Dialog) -> Option<u32> {
    use sip_message::SipParser;
    let handle = dialog.ext.pending_invite_txn.as_ref()?;
    match sip_message::parser::custom::CustomParser::new()
        .parse(&handle.original_invite)
        .ok()?
    {
        sip_message::SipMessage::Request(r) => Some(r.cseq().seq()),
        _ => None,
    }
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The one Route line the request carries.
    fn top_route(req: &SipRequest) -> String {
        req.raw(HeaderName::Route).next().expect("route header").to_string()
    }

    fn in_dialog_options(route: &str) -> SipRequest {
        parse(&format!(
            "OPTIONS sip:sipp@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKa;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:sipp@10.244.2.7:5060>;tag=uac\r\n\
Call-ID: c1@x\r\n\
CSeq: 2 OPTIONS\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
        ))
    }

    // A worker-originated in-dialog request loose-routes back through our front
    // proxy on the route set captured at dialog set-up. The worker no longer
    // stamps anything: under double-record-routing the worker-facing half of that
    // route set is ALREADY the proxy's own `;outbound` Record-Route, so egress
    // just forwards the top Route verbatim and resolves the wire destination to
    // it. (The proxy reads direction from its own self-issued RR — registry- and
    // pod-IP-independent, so it survives a worker reboot.)
    #[test]
    fn worker_in_dialog_loose_route_is_forwarded_verbatim() {
        let route = "<sip:10.0.0.9:5060;outbound;lr>";
        let (out, dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "a",
            &[route.to_string()],
            in_dialog_options(route),
            ("10.244.2.7".to_string(), 5060),
        );
        // The top Route is unchanged (the proxy issued the `;outbound`, not us).
        assert_eq!(top_route(&out), route, "egress must forward the captured route set verbatim");
        // Loose route → wire destination is the proxy (top route); R-URI unchanged.
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060));
    }

    // The worker does NOT add `;outbound` to a cookie route it did not issue: a
    // route set whose top is the proxy's stickiness cookie (no `;outbound`) is
    // forwarded untouched (this is the EXTERNAL-facing half — it should never be
    // on top of a worker-originated request, but egress must not mutate it).
    #[test]
    fn worker_in_dialog_does_not_stamp_outbound() {
        let route = "<sip:10.0.0.9:5060;target=10.244.1.5:5060;lr>";
        let (out, dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "a",
            &[route.to_string()],
            in_dialog_options(route),
            ("10.244.2.7".to_string(), 5060),
        );
        let forwarded = top_route(&out);
        let uri = loose_route_uri(&forwarded).expect("a loose route");
        assert!(
            uri.param("outbound").is_none(),
            "egress must not stamp ;outbound; got {forwarded}"
        );
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060));
    }

    // The pre-confirmation b-leg INVITE (empty route set) preloads a PLAIN loose
    // Route to the outbound proxy — no `;outbound`. The proxy classifies the
    // initial INVITE worker-outbound from the top Via (the originating worker is
    // live at set-up) and double-record-routes the dialog from there.
    #[test]
    fn b_leg_bootstrap_preloads_plain_loose_route() {
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        let invite = parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKb;lg=b\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: c2@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        );
        let (out, dest) = apply_b_leg_egress(&config, "b-1", &[], invite, ("10.244.2.7".to_string(), 5060));
        let preloaded = top_route(&out);
        assert_eq!(preloaded, "<sip:10.0.0.9:5060;lr>", "bootstrap preload must be a plain loose Route");
        let uri = loose_route_uri(&preloaded).expect("a loose route");
        assert!(uri.param("outbound").is_none(), "no ;outbound on the bootstrap preload");
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060), "wire destination is the outbound proxy");
    }

    // `leg_egress_dest` mirrors apply_b_leg_egress's destination decision WITHOUT
    // mutating a request — used for keepalive-timeout peer attribution. It must
    // agree with the BYE path on every branch: loose-route → top route host; empty
    // route-set b-leg + outbound proxy → the proxy; a-leg / no proxy → base.
    #[test]
    fn leg_egress_dest_mirrors_apply_b_leg_egress() {
        let base = ("10.244.2.7".to_string(), 5060);

        // (1) Loose route on top → wire dest is the top route's host:port.
        let route = "<sip:10.0.0.9:5060;outbound;lr>".to_string();
        assert_eq!(
            leg_egress_dest(&B2buaConfig::default(), "b-1", &[route.clone()], base.clone()),
            ("10.0.0.9".to_string(), 5060),
            "loose route → top route host:port",
        );
        // Agrees with the request-mutating path's destination.
        let (_, mut_dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "b-1",
            &[route],
            in_dialog_options("<sip:10.0.0.9:5060;outbound;lr>"),
            base.clone(),
        );
        assert_eq!(mut_dest, ("10.0.0.9".to_string(), 5060));

        // (2) Empty route set, b-leg, outbound proxy configured → the proxy.
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        assert_eq!(
            leg_egress_dest(&config, "b-1", &[], base.clone()),
            ("10.0.0.9".to_string(), 5060),
            "empty route-set b-leg + outbound proxy → the proxy",
        );

        // (3) a-leg with empty route set → the base remote target (the proxy
        // bootstrap is a b-leg-only concept).
        assert_eq!(
            leg_egress_dest(&config, "a", &[], base.clone()),
            base.clone(),
            "a-leg → base remote target (no proxy bootstrap on the a-leg)",
        );

        // (4) b-leg, empty route set, NO outbound proxy → the base.
        assert_eq!(
            leg_egress_dest(&B2buaConfig::default(), "b-1", &[], base.clone()),
            base,
            "no outbound proxy → base remote target",
        );
    }
}

#[cfg(test)]
mod identity_tests {
    //! Per-leg identity invariants (ID-1/2/3, HDR-2) — the core back-to-back-UA
    //! property: the B2BUA mints the b-leg's dialog identity from scratch, copying
    //! *nothing* dialog-identifying from the a-leg. A b-leg whose Call-ID / From-tag
    //! / CSeq / Contact leaked from the a-leg would couple the two dialogs and is
    //! exactly what a transparent proxy (not a B2BUA) would do. Asserted directly
    //! against [`build_b_leg`] (the single mint point, `relay.rs`).
    use super::*;
    use sip_message::header::Contact;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// A representative inbound a-leg INVITE with its OWN Call-ID, From-tag, an
    /// absent To-tag (initial INVITE), CSeq 314 and a caller-owned Contact user.
    fn a_leg_invite() -> SipRequest {
        parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        )
    }

    /// The b-leg INVITE's Contact.
    fn contact_of(req: &SipRequest) -> Contact {
        req.header::<Contact>().expect("b-leg INVITE has a Contact").expect("a readable Contact")
    }

    // The b-leg INVITE's dialog identity is independent of the a-leg's: a fresh
    // Call-ID, a fresh From-tag, no To-tag, CSeq 1, and the B2BUA's own Contact.
    #[test]
    fn b_leg_identity_is_independent_of_a_leg() {
        let a = a_leg_invite();
        let config = B2buaConfig::default(); // sip_local_ip = 127.0.0.1
        let id_gen = IdGen::seeded(0xB2B);

        let (leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false, // non-emergency
            &a,
            ("10.244.2.7".to_string(), 5060),
            None, // R-URI defaults to a-leg's
            None, // From URI relayed from a-leg
            None, // To URI relayed from a-leg
            None, // no NoAnswer
            &config,
            &id_gen,
            None, // no body override
            &[],  // no header updates
            &CapabilitySet::default(), // undeclared → the stack capability set
            None,
            None, // Destination leg
        )
        .expect("no identity rewrites, so nothing to refuse");

        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };

        // (a) ID-1 — fresh Call-ID, NOT the a-leg's.
        let call_id = invite.call_id();
        let call_id = call_id.as_str();
        assert_ne!(call_id, a.call_id().as_str(), "b-leg Call-ID must not be the a-leg's");
        assert_eq!(call_id, leg.call_id, "leg + INVITE Call-IDs agree");
        // The mint shape is `<leg>-<tag>@<local_ip>` (relay.rs), so it carries the
        // leg id and the B2BUA's own host — never alice's Call-ID host.
        assert!(call_id.starts_with("b-1-"), "Call-ID carries the leg id: {call_id}");
        assert!(call_id.ends_with("@127.0.0.1"), "Call-ID host is the B2BUA's: {call_id}");

        // (b) ID-2 — fresh From-tag (B2BUA-owned), NOT the a-leg's; To-tag absent
        // on the initial INVITE (the callee mints it in its 2xx, RFC 3261 §12.1.1).
        let from = invite.from();
        let from_tag = from.tag().expect("b-leg From carries a tag");
        assert_ne!(from_tag, "alice-from-tag", "b-leg From-tag must not be the a-leg's");
        assert_eq!(from_tag, leg.from_tag, "leg + INVITE From-tags agree");
        let to = invite.to();
        assert!(to.tag().is_none(), "initial b-leg INVITE has no To-tag, got {:?}", to.tag());

        // (c) ID-3 — the b-leg dialog starts a fresh CSeq space at 1 (the a-leg's
        // INVITE was CSeq 314).
        assert_eq!(invite.cseq().seq(), 1, "b-leg CSeq starts at 1, independent of the a-leg's 314");

        // (d) HDR-2 — Contact is the B2BUA's own address (host = local_ip), and its
        // user is the B2BUA's, NOT the a-leg caller's ("alice").
        let contact = contact_of(&invite);
        let cuser = contact.uri().user().unwrap_or("");
        assert_ne!(cuser, "alice", "b-leg Contact user must not be the a-leg caller's");
        assert_eq!(cuser, "b2bua", "b-leg Contact is the B2BUA's own identity");
        assert_eq!(
            contact.uri().host(),
            "127.0.0.1",
            "b-leg Contact host must be the B2BUA local addr: {}",
            contact.uri()
        );
    }

    /// Build the initial b-leg INVITE for `is_emergency` and return its raw Via +
    /// Contact header values (the on-the-wire surface, not the builder structs).
    fn b_leg_invite_via_contact(is_emergency: bool) -> (String, String) {
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            is_emergency,
            &a_leg_invite(),
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xE3E),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        (
            invite.top_via().to_string(),
            contact_of(&invite).to_wire(),
        )
    }

    // The wiring contract: an EMERGENCY call's initial b-leg INVITE (the single
    // mint point) carries `;em=1` on its Via and `;emerg=1` on its Contact ON
    // THE WIRE, so the call stays identifiable in-dialog. The pure-builder tests
    // prove the `if is_emergency` branch in isolation; this proves a production
    // relay path actually passes `true` and the markers reach the serialized
    // message.
    #[test]
    fn emergency_b_leg_invite_via_and_contact_carry_the_markers() {
        let (via, contact) = b_leg_invite_via_contact(true);
        assert!(via.contains(";em=1"), "emergency b-leg Via must carry ;em=1: {via}");
        assert!(
            contact.contains(";emerg=1"),
            "emergency b-leg Contact must carry ;emerg=1: {contact}"
        );
    }

    // A non-emergency call's b-leg INVITE carries NEITHER marker (the markers are
    // strictly an emergency signal — stamping them on a normal call would exempt
    // it from overload shedding).
    #[test]
    fn non_emergency_b_leg_invite_omits_the_markers() {
        let (via, contact) = b_leg_invite_via_contact(false);
        assert!(!via.contains(";em=1"), "non-emergency Via must NOT carry ;em=1: {via}");
        assert!(
            !contact.contains(";emerg=1"),
            "non-emergency Contact must NOT carry ;emerg=1: {contact}"
        );
    }

    /// An a-leg INVITE carrying, alongside its structural headers: an extension
    /// header and an unmodelled vendor one (both relayable), a `Record-Route`
    /// (alice's route set is not the callee's to learn), and one member of each
    /// withheld class.
    fn a_leg_invite_with_relay_header() -> SipRequest {
        parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
Record-Route: <sip:proxy.alice.example;lr>\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
X-Loadgen-Id: lg-abc123\r\n\
P-Charging-Vector: icid-value=\"icid-from-alice\"\r\n\
Authorization: Digest username=\"alice\",realm=\"alice.example\"\r\n\
Session-Expires: 1800;refresher=uac\r\n\
Timestamp: 54\r\n\
Replaces: other-call-id;to-tag=t;from-tag=f\r\n\
Require: precondition\r\n\
Content-Length: 0\r\n\r\n",
        )
    }

    /// Build the originated b-leg INVITE for the given relay config + R-URI and
    /// return its parsed headers. `new_ruri` lets one helper drive BOTH the
    /// normal callee leg (`None` → keeps the a-leg R-URI / bob) and the REFER
    /// transfer leg (`Some(charlie)` → the rebuilt a-leg invite re-aimed).
    fn relay_b_leg_headers(relay_headers: Vec<String>, new_ruri: Option<&str>) -> Vec<MsgHeader> {
        let config = B2buaConfig { relay_headers, ..B2buaConfig::default() };
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite_with_relay_header(),
            ("10.244.2.7".to_string(), 5060),
            new_ruri,
            None,
            None,
            None,
            &config,
            &IdGen::seeded(0x4747),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .expect("the R-URI under test reads");
        match effect.body {
            OutboundBody::Request(r) => r.headers().to_vec(),
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        }
    }

    /// RFC 3261 §16.6: a header alice sent that the generator does not own rides
    /// onto BOTH originated legs — the callee leg and the REFER transfer leg,
    /// which is the same mint point fed alice's rehydrated INVITE. The
    /// generator-owned headers stay the generator's: naming `To` in the
    /// configured list still yields exactly one, structurally minted `To`, and
    /// alice's `Record-Route`/`Contact`/`Via` never reach the callee.
    #[test]
    fn a_received_header_rides_both_originated_legs_and_never_the_owned_ones() {
        let has = |hs: &[MsgHeader], name: &str, val: &str| {
            hs.iter().any(|h| h.name.eq_ignore_ascii_case(name) && h.value == val)
        };
        let count = |hs: &[MsgHeader], name: &str| {
            hs.iter().filter(|h| h.name.eq_ignore_ascii_case(name)).count()
        };

        // (a) NORMAL callee leg (bob), with nothing configured: the relay carries
        //     the extension header and the vendor header it has no model for.
        let bob = relay_b_leg_headers(Vec::new(), None);
        assert!(has(&bob, "X-Loadgen-Id", "lg-abc123"), "callee leg carries it: {bob:?}");
        assert!(
            has(&bob, "P-Charging-Vector", "icid-value=\"icid-from-alice\""),
            "an unmodelled header rides verbatim: {bob:?}"
        );

        // (b) REFER transfer leg (charlie): same mint point, R-URI re-aimed.
        let charlie = relay_b_leg_headers(Vec::new(), Some("sip:charlie@10.244.2.9:5060"));
        assert!(has(&charlie, "X-Loadgen-Id", "lg-abc123"), "transfer leg carries it");

        // (c) The generator's own headers are the generator's: alice's route set
        //     and her Contact are hers, and her Via would be a routing loop.
        assert_eq!(count(&bob, "Record-Route"), 0, "alice's route set stays alice's: {bob:?}");
        assert_eq!(count(&bob, "Via"), 1, "exactly the b-leg's own Via: {bob:?}");
        assert!(
            !has(&bob, "Contact", "<sip:alice@192.0.2.5:5060>"),
            "the b-leg Contact is this stack's: {bob:?}"
        );

        // (d) Naming a generator-owned header in the configured list cannot
        //     duplicate it — configuration does not reach past §16.6.
        let with_to = relay_b_leg_headers(vec!["To".into()], None);
        assert_eq!(count(&with_to, "To"), 1, "one structural To, never a relayed dup: {with_to:?}");
    }

    /// The withheld classes do not ride the originated INVITE: a credential
    /// scoped to alice's realm, a session interval negotiated with alice, her
    /// own clock stamp, a dialog identifier this stack re-mints, and a
    /// requirement this stack already accepted as the UAS.
    #[test]
    fn the_withheld_classes_never_reach_the_callee() {
        let bob = relay_b_leg_headers(Vec::new(), None);
        for name in
            ["Authorization", "Session-Expires", "Timestamp", "Replaces", "Require"]
        {
            assert!(
                !bob.iter().any(|h| h.name.eq_ignore_ascii_case(name)),
                "{name} must not reach the callee: {bob:?}"
            );
        }
    }

    /// Precedence at the originated-leg mint point: a decision's explicit header
    /// update is more specific than the relayed value and wins, as exactly one
    /// line of that name.
    #[test]
    fn an_explicit_header_update_beats_the_relayed_value() {
        let updates =
            vec![("X-Loadgen-Id".to_string(), Some("stated-by-the-decision".to_string()))];
        let bob = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite_with_relay_header(),
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0x4747),
            None,
            &updates,
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .map(|(_leg, effect)| match effect.body {
            OutboundBody::Request(r) => r.headers().to_vec(),
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        })
        .expect("no identity rewrites, so nothing to refuse");
        let stated: Vec<&str> = bob
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("X-Loadgen-Id"))
            .map(|h| h.value.as_str())
            .collect();
        assert_eq!(stated, ["stated-by-the-decision"], "the update wins, alone: {bob:?}");
    }
}

#[cfg(test)]
mod response_transparency_tests {
    //! What a b-leg response carries onto the response minted toward the
    //! originator (RFC 3261 §16.6) — the set every relay exit shares.
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// A callee 183 carrying the reliable-provisional negotiation, an early-media
    /// authorization, a release cause, a body and the header describing it, plus
    /// the callee's own route set and clock stamp.
    fn b_leg_183() -> sip_message::SipResponse {
        let raw = "SIP/2.0 183 Session Progress\r\n\
Via: SIP/2.0/UDP 10.244.2.7:5080;branch=z9hG4bK-b\r\n\
Record-Route: <sip:proxy.bob.example;lr>\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob-tag\r\n\
Call-ID: b-leg-call-id\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Require: 100rel\r\n\
RSeq: 1\r\n\
Supported: 100rel, timer\r\n\
P-Early-Media: sendrecv\r\n\
Reason: Q.850;cause=17\r\n\
Timestamp: 54\r\n\
Content-Disposition: session;handling=required\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected response"),
        }
    }

    fn names(headers: &[MsgHeader]) -> Vec<String> {
        headers.iter().map(|h| h.name.to_ascii_lowercase()).collect()
    }

    /// The callee's end-to-end headers reach the caller — the RFC 3262
    /// negotiation, the RFC 5009 early-media authorization and the Q.850 cause
    /// alike — while the callee's route set, Contact and clock stamp do not.
    #[test]
    fn a_relayed_response_carries_the_callee_end_to_end_set() {
        let carried = names(&relay_response_passthrough_headers(&b_leg_183(), true));
        for name in ["require", "rseq", "supported", "p-early-media", "reason"] {
            assert!(carried.contains(&name.to_string()), "{name} must ride: {carried:?}");
        }
        for name in ["via", "record-route", "contact", "to", "content-type", "timestamp"] {
            assert!(!carried.contains(&name.to_string()), "{name} must not ride: {carried:?}");
        }
    }

    /// A policy that drops or replaces the body leaves the header describing
    /// that body behind: `handling=required` must not describe a body the caller
    /// never receives.
    #[test]
    fn body_metadata_does_not_outlive_the_body_it_describes() {
        let with_body = names(&relay_response_passthrough_headers(&b_leg_183(), true));
        assert!(with_body.contains(&"content-disposition".to_string()));

        let without = names(&relay_response_passthrough_headers(&b_leg_183(), false));
        assert!(!without.contains(&"content-disposition".to_string()), "{without:?}");
        assert!(without.contains(&"p-early-media".to_string()), "the rest still rides: {without:?}");
    }
}

#[cfg(test)]
mod unreadable_address_tests {
    //! upstreamneed-055: a decision-supplied address that no reader accepts is a
    //! refusal, never a fabricated value. The failure mode this replaces was
    //! silent — the B2BUA built an *opaque* URI whose host was the whole raw
    //! text and originated a b-leg toward it, so the call either died later as
    //! an unattributable transport error or reached a destination nobody named.
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn a_leg_invite() -> SipRequest {
        let raw = "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// Build a b-leg with the three identity rewrites set as given.
    fn build(
        new_ruri: Option<&str>,
        new_from: Option<&str>,
        new_to: Option<&str>,
    ) -> Result<(Leg, OutboundSipEffect), UnreadableAddress> {
        build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite(),
            ("10.244.2.7".to_string(), 5060),
            new_ruri,
            new_from,
            new_to,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0x055),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
    }

    /// Addresses RFC 3261 §19.1.1 refuses, each of which `Uri::opaque` would
    /// have turned into a "host" equal to the whole string.
    const UNREADABLE: &[&str] = &[
        "sip:2001:db8::1",            // unbracketed IPv6 — would resolve "2001"
        "sip:host:88161",             // port out of range
        "not a uri at all",           // no scheme
        "sip:[2001:db8::1",           // unclosed IPv6 reference
    ];

    // Every identity rewrite is refused, and the error names WHICH field —
    // without that the operator sees a resolution failure with no defect in it.
    #[test]
    fn an_unreadable_identity_rewrite_refuses_the_leg_and_names_the_field() {
        for text in UNREADABLE {
            for (field, built) in [
                ("new_ruri", build(Some(text), None, None)),
                ("new_from", build(None, Some(text), None)),
                ("new_to", build(None, None, Some(text))),
            ] {
                let err = built.err().unwrap_or_else(|| {
                    panic!("{field}={text:?} must be refused, not routed")
                });
                assert_eq!(err.field, field);
                assert_eq!(err.value, *text, "the refusal carries the stated text");
                // The wire form names the field only — a peer's bytes never
                // reach a reason phrase through it.
                assert_eq!(err.to_string(), format!("Unreadable Routing Address ({field})"));
                assert!(err.detail().contains(text), "the log detail keeps the value");
            }
        }
    }

    // The refusal is exact: a readable rewrite still builds, and the b-leg
    // carries it. A blanket refusal would break every identity rewrite.
    #[test]
    fn a_readable_identity_rewrite_still_builds_the_leg() {
        let (leg, effect) = build(
            Some("sip:charlie@10.244.2.9:5060"),
            Some("sip:+15551234@carrier.example"),
            Some("sip:+15559876@carrier.example"),
        )
        .expect("a readable rewrite must route");
        assert_eq!(leg.invite_request_uri.as_deref(), Some("sip:charlie@10.244.2.9:5060"));
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        assert_eq!(invite.from().uri().host(), "carrier.example");
        assert_eq!(invite.to().uri().user(), Some("+15559876"));
    }

    // No rewrites at all: the relayed a-leg values were admitted by the inbound
    // parser, so nothing is re-read and nothing can be refused.
    #[test]
    fn relayed_a_leg_addresses_are_never_refused() {
        let (_leg, effect) = build(None, None, None).expect("relayed a-leg values must route");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        assert_eq!(invite.request_uri().host_port(), ("10.244.2.7", 5060));
    }

    // A 3xx Contact is what the caller dials next, so an unreadable redirect
    // target is refused rather than emitted as an opaque URI.
    #[test]
    fn an_unreadable_redirect_target_is_refused() {
        for text in UNREADABLE {
            let err = redirect_contact(text, Some(0.5))
                .err()
                .unwrap_or_else(|| panic!("redirect target {text:?} must be refused"));
            assert_eq!(err.field, "contact");
        }
        let header = redirect_contact("sip:carol@10.244.2.11:5060", Some(0.7))
            .expect("a readable redirect target must render");
        assert!(header.value.contains("sip:carol@10.244.2.11:5060"));
        assert!(header.value.contains("q=0.7"));
    }
}

#[cfg(test)]
mod advertisement_tests {
    //! What the two B2BUA-owned INVITE mint points advertise (`Allow` /
    //! `Supported`): [`build_b_leg`] on the originated leg and
    //! [`stamp_a_facing_invite_advert`] on the response facing the originator.
    //! Every assertion reads the emitted header, not the declaration.
    use super::*;
    use sip_message::header::{Allow, Supported};
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn a_leg_invite() -> SipRequest {
        a_leg_invite_carrying(&[])
    }

    /// Alice's INVITE, carrying `extra` header lines of her own.
    pub(super) fn a_leg_invite_carrying(extra: &[(&str, &str)]) -> SipRequest {
        let mut raw = "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n"
            .to_string();
        for (name, value) in extra {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("Content-Length: 0\r\n\r\n");
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The `(Allow, Supported)` the originated-leg INVITE carries on the wire.
    fn b_leg_advert(
        capabilities: &CapabilitySet,
        header_updates: &[(String, Option<String>)],
    ) -> (Option<String>, Option<String>) {
        b_leg_advert_from(&a_leg_invite(), capabilities, header_updates)
    }

    /// The same, for an originator INVITE that advertises a set of its own.
    fn b_leg_advert_from(
        a_leg_invite: &SipRequest,
        capabilities: &CapabilitySet,
        header_updates: &[(String, Option<String>)],
    ) -> (Option<String>, Option<String>) {
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            a_leg_invite,
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xCAB),
            None,
            header_updates,
            capabilities,
            None, // no charging vector
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        let allow = invite.raw_text(HeaderName::Allow).next().map(|v| v.as_str().to_string());
        let supported =
            invite.raw_text(HeaderName::Supported).next().map(|v| v.as_str().to_string());
        (allow, supported)
    }

    /// The `(Allow, Supported)` the a-facing 2xx header set ends up carrying,
    /// starting from a callee value the B2BUA must replace.
    fn a_facing_advert(
        capabilities: &CapabilitySet,
        rule_stamped: &[Entry],
    ) -> (Option<String>, Option<String>) {
        let mut headers = vec![MsgHeader {
            name: SipStr::from_static("Supported"),
            value: SipStr::from_static("callees-own-tag"),
        }];
        stamp_a_facing_invite_advert(&mut headers, rule_stamped, capabilities);
        let value = |name: HeaderName| {
            headers.iter().find(|h| name.matches(&h.name)).map(|h| h.value.as_str().to_string())
        };
        (value(HeaderName::Allow), value(HeaderName::Supported))
    }

    /// An inbound re-INVITE carrying the peer's own `Supported`.
    fn peer_reinvite() -> SipRequest {
        let raw = "INVITE sip:b2bua@10.244.2.7:5080 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-reinvite\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=b2bua-to-tag\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 315 INVITE\r\n\
Supported: 100rel, timer, replaces\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The `(Allow, Supported)` a RELAYED re-INVITE carries toward the target
    /// face, built exactly as the relay action builds it: the passthrough set
    /// as `extra_headers`, the face's set as `capabilities`.
    fn relayed_reinvite_advert(
        capabilities: &CapabilitySet,
        target_declared: &[HeaderName],
    ) -> (Option<String>, Option<String>) {
        let dialog = generators::StackDialog {
            call_id: "b-leg-call-id".to_string(),
            local_tag: "b2bua-local".to_string(),
            remote_tag: "bob-remote".to_string(),
            local_uri: "sip:b2bua@10.244.2.7:5080".to_string(),
            remote_uri: "sip:bob@10.0.0.2:5070".to_string(),
            remote_target: "sip:bob@10.0.0.2:5070".to_string(),
            local_cseq: 41,
            route_set: Vec::new(),
        };
        let opts = generators::GenerateInDialogRequestOpts {
            via: Some(
                Via::parse(&SipStr::from_static("SIP/2.0/UDP 10.244.2.7:5080;branch=z9hG4bK-b"))
                    .unwrap(),
            ),
            contact: Some(
                header::Contact::parse(&SipStr::from_static("<sip:b2bua@10.244.2.7:5080>"))
                    .unwrap(),
            ),
            extra_headers: relay_request_passthrough_headers(&peer_reinvite(), target_declared),
            capabilities: Some(capabilities.clone()),
            ..Default::default()
        };
        let out = generators::generate_in_dialog_request(
            generators::InDialogMethod::Invite,
            &dialog,
            &opts,
        )
        .request;
        let value =
            |name: HeaderName| out.raw_text(name).next().map(|v| v.as_str().to_string());
        (value(HeaderName::Allow), value(HeaderName::Supported))
    }

    /// A narrow set: no REFER/INFO/NOTIFY/PRACK, no 100rel, no timers.
    fn narrow() -> CapabilitySet {
        CapabilitySet::new(
            Allow::of(["INVITE", "ACK", "CANCEL", "BYE", "OPTIONS"]),
            Supported::of(["replaces"]),
        )
    }

    /// Declaring nothing advertises the stack set on BOTH faces, byte for byte.
    #[test]
    fn an_undeclared_call_advertises_the_stack_set_on_both_faces() {
        let default = CapabilitySet::default();
        let (allow, supported) = b_leg_advert(&default, &[]);
        assert_eq!(allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(supported.as_deref(), Some(generators::B2BUA_SUPPORTED));

        let (allow, supported) = a_facing_advert(&default, &[]);
        assert_eq!(allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(supported.as_deref(), Some(generators::B2BUA_SUPPORTED));
    }

    /// A declared set is what reaches the wire on the originated leg.
    #[test]
    fn the_declared_set_reaches_the_originated_leg() {
        let (allow, supported) = b_leg_advert(&narrow(), &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// A declared set is what reaches the wire toward the originator, replacing
    /// whatever the callee's 200 carried.
    #[test]
    fn the_declared_set_reaches_the_originator() {
        let (allow, supported) = a_facing_advert(&narrow(), &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// The two faces are independent: a bridge between asymmetric domains
    /// narrows the originated leg while the originator still sees the full set.
    #[test]
    fn the_two_faces_advertise_independently() {
        let (b_allow, b_supported) = b_leg_advert(&narrow(), &[]);
        let (a_allow, a_supported) = a_facing_advert(&CapabilitySet::default(), &[]);
        assert_eq!(b_allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(b_supported.as_deref(), Some("replaces"));
        assert_eq!(a_allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(a_supported.as_deref(), Some(generators::B2BUA_SUPPORTED));
        assert_ne!(a_allow, b_allow, "the faces carry different Allow sets");
    }

    /// An explicit header update on the message is more specific than the
    /// call's declared set and wins; the declaration still supplies the header
    /// the update does not name.
    #[test]
    fn an_explicit_header_update_beats_the_declared_set_on_the_originated_leg() {
        let updates =
            vec![("Allow".to_string(), Some("INVITE, ACK, BYE, MESSAGE".to_string()))];
        let (allow, supported) = b_leg_advert(&narrow(), &updates);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, BYE, MESSAGE"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// Same precedence toward the originator: a value the firing rule stamped
    /// itself wins over the declared set, and stays the ONLY line of that name.
    #[test]
    fn a_rule_stamped_value_beats_the_declared_set_toward_the_originator() {
        let stamped = [Entry::typed(Supported::of(["timer"]))];
        let (allow, supported) = a_facing_advert(&narrow(), &stamped);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(
            supported.as_deref(),
            Some("callees-own-tag"),
            "the rule owns Supported here, so the pass-through line it placed is kept as-is"
        );
    }

    /// A declared set survives the RELAYED re-INVITE: the peer's `Supported`
    /// is a relayed value, not an explicit instruction, so it does not revert
    /// the narrowing that `Allow` (never relayed) keeps on the same message.
    #[test]
    fn a_declared_set_survives_a_relayed_reinvite() {
        let (allow, supported) = relayed_reinvite_advert(&narrow(), &[HeaderName::Supported]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// With no option-tag declaration the transparent relay stands: the peer's
    /// `Supported` still rides end to end (RFC 3262 negotiation).
    #[test]
    fn an_undeclared_supported_still_relays_the_peers_value_on_a_reinvite() {
        let (allow, supported) = relayed_reinvite_advert(&CapabilitySet::default(), &[]);
        assert_eq!(allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(supported.as_deref(), Some("100rel, timer, replaces"));
    }

    /// The originator's own advertisement travels onto the leg the B2BUA
    /// originates (RFC 3261 §16.6): every method she accepts is still there,
    /// with the stack's own added — the face accepts those too.
    #[test]
    fn the_originators_methods_reach_the_originated_leg_with_the_stacks_added() {
        let invite = a_leg_invite_carrying(&[("Allow", "INVITE, ACK, BYE, MESSAGE")]);
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (allow, _) = b_leg_advert_from(&invite, &caps, &[]);
        let allow = allow.expect("the originated leg advertises its methods");
        for method in ["INVITE", "ACK", "BYE", "MESSAGE"] {
            assert!(allow.contains(method), "{method} the originator accepts must survive");
        }
        assert!(allow.contains("PRACK"), "a method the stack services is added");
    }

    /// An option tag obliges whoever advertises it: the originated leg claims
    /// exactly what the originator claimed — the captured tag is not dropped,
    /// and `100rel`/`timer` are not invented on her behalf.
    #[test]
    fn the_originated_leg_claims_the_originators_option_tags_and_no_others() {
        let invite = a_leg_invite_carrying(&[("Supported", "path, gin")]);
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (_, supported) = b_leg_advert_from(&invite, &caps, &[]);
        assert_eq!(supported.as_deref(), Some("path, gin"));
    }

    /// A declaration is the more specific statement and still outranks the
    /// relayed set, half for half.
    #[test]
    fn a_declaration_outranks_the_originators_relayed_set() {
        let invite =
            a_leg_invite_carrying(&[("Allow", "INVITE, MESSAGE"), ("Supported", "path")]);
        let features = declaring_originated(Some(vec!["INVITE".into(), "ACK".into()]), None);
        let caps = crate::rules::capabilities::relaying_in(
            Some(&features),
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (allow, supported) = b_leg_advert_from(&invite, &caps, &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK"), "the declared half stands");
        assert_eq!(supported.as_deref(), Some("path"), "the undeclared half relays");
    }

    /// Feature activations declaring a set toward the originated face.
    fn declaring_originated(
        allow: Option<Vec<String>>,
        supported: Option<Vec<String>>,
    ) -> call::features::FeatureActivations {
        call::features::FeatureActivations {
            platform: call::features::PlatformActivations {
                max_duration_sec: 3_600,
                keepalive: call::features::KeepaliveActivation {
                    interval_sec: 30,
                    max_missed: 2,
                },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            call_limiters: None,
            charging_vector: None,
            advertise_capabilities: Some(call::features::AdvertiseCapabilitiesFeature {
                toward_originator: None,
                toward_originated: Some(call::features::AdvertisedCapabilities {
                    allow,
                    supported,
                }),
            }),
        }
    }
}

#[cfg(test)]
mod charging_tests {
    //! RFC 7315 §5.6 charging correlation on a leg the B2BUA originates.
    use super::advertisement_tests::a_leg_invite_carrying;
    use super::*;
    use call::features::ChargingVectorFeature;

    /// The `P-Charging-Vector` the originated-leg INVITE carries, if any.
    fn b_leg_vector(
        a_leg_invite: &SipRequest,
        charging: Option<&ChargingVectorFeature>,
    ) -> Option<String> {
        b_leg_vector_from(a_leg_invite, charging, &IdGen::seeded(0xCAB))
    }

    /// The same, on a stated generator — one worker's identifier stream.
    fn b_leg_vector_from(
        a_leg_invite: &SipRequest,
        charging: Option<&ChargingVectorFeature>,
        id_gen: &IdGen,
    ) -> Option<String> {
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            a_leg_invite,
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            id_gen,
            None,
            &[],
            &CapabilitySet::default(),
            charging,
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        let name = ChargingVector::header_name();
        invite
            .headers()
            .iter()
            .find(|h| name.matches(&h.name))
            .map(|h| h.value.as_str().to_string())
    }

    /// Armed and nothing received: the element starting the leg generates the
    /// identifier, stating where it generated it.
    #[test]
    fn an_originated_leg_carries_a_generated_identifier() {
        let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&ChargingVectorFeature::default()))
            .expect("an armed call stamps a charging vector");
        let parsed = ChargingVector::parse(&SipStr::owned(&value)).expect("RFC 7315 §5.6 form");
        assert!(!parsed.icid_value().is_empty());
        assert_eq!(parsed.icid_generated_at(), Some(B2buaConfig::default().sip_local_ip.as_str()));
    }

    /// Two legs of two calls never share an identifier — it is the key the
    /// records are matched on.
    #[test]
    fn each_originated_leg_generates_its_own_identifier() {
        let arm = ChargingVectorFeature::default();
        let id_gen = IdGen::seeded(0xCAB);
        let first = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &id_gen);
        let second = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &id_gen);
        assert_ne!(first, second);
    }

    /// The correlation invariant: a vector the originator sent is the session's
    /// identifier, relayed unchanged — an armed call never re-mints it.
    #[test]
    fn a_received_identifier_is_relayed_unchanged_even_when_armed() {
        let received = "icid-value=abc123;icid-generated-at=upstream.example";
        let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
        assert_eq!(
            b_leg_vector(&invite, Some(&ChargingVectorFeature::default())).as_deref(),
            Some(received)
        );
    }

    /// Unarmed: the stack generates none, and a received one still relays.
    #[test]
    fn an_unarmed_call_generates_none() {
        assert_eq!(b_leg_vector(&a_leg_invite_carrying(&[]), None), None);
        let received = "icid-value=abc123";
        let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
        assert_eq!(b_leg_vector(&invite, None).as_deref(), Some(received));
    }

    /// The arm names the element the identifier is generated at.
    #[test]
    fn the_arm_names_the_generating_element() {
        let arm = ChargingVectorFeature { generated_at: Some("edge.example".to_string()) };
        let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&arm)).expect("armed");
        let parsed = ChargingVector::parse(&SipStr::owned(&value)).unwrap();
        assert_eq!(parsed.icid_generated_at(), Some("edge.example"));
    }
}
