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
use sip_message::generators::{
    self, ContactSpec, GenerateAckFor2xxOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    OutOfDialogMethod, ViaSpec,
};
use sip_message::message_helpers::get_header;
use sip_message::{hydrate_request, SipHeader as MsgHeader, SipMessage, SipRequest};
use sip_txn::{IdGen, TxnKind};

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::stack_identity::{build_call_contact, build_call_via, StackIdentityOpts};

/// Convert `call`-crate headers to message-crate headers.
pub fn to_msg_headers(headers: &[call::SipHeader]) -> Vec<MsgHeader> {
    headers
        .iter()
        .map(|h| MsgHeader {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect()
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
pub fn rebuild_a_leg_invite(snap: &call::ALegInviteSnapshot) -> SipRequest {
    hydrate_request(
        "INVITE",
        &snap.uri,
        to_msg_headers(&snap.headers),
        snap.body.clone(),
    )
    .expect("a-leg INVITE snapshot is well-formed")
}

/// The B2BUA's Via for a leg's outbound message. `is_emergency` is the call's
/// emergency state (`call.emergency == Some(true)`); when set it stamps the
/// `;em=1` marker every subsequent in-dialog packet then carries — the in-dialog
/// signal the Tier-1 overload brake (`buffer_has_emergency_marker`) scans to
/// never 503 an admitted emergency call. Port of `legStackIdentity`'s
/// `isEmergency = call.emergency === true` (stack-identity.ts L137).
pub fn leg_via(
    config: &B2buaConfig,
    call_ref: &str,
    leg_id: &str,
    is_emergency: bool,
    branch: String,
) -> ViaSpec {
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
) -> ContactSpec {
    build_call_contact(&StackIdentityOpts {
        local_ip: &config.sip_local_ip,
        local_port: config.sip_local_port,
        call_ref,
        leg: leg_id,
        is_emergency,
    })
}

/// Parse `"host:port"` (or `host`) into `(host, port)`.
pub fn dest_of(uri_host_port: &str) -> (String, u16) {
    let hp = uri_host_port.trim();
    if let Some((h, p)) = hp.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.to_string(), port);
        }
    }
    (hp.to_string(), 5060)
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
    mut req: SipRequest,
    dest: (String, u16),
) -> (SipRequest, (String, u16)) {
    // (1) Loose-route: send to the top route's host:port (R-URI unchanged).
    if let Some(first) = route_set.first() {
        if generators::first_route_is_loose(first) {
            // The route value is an angle-bracketed name-addr (`<sip:host;lr>`);
            // unwrap to the bare URI before reducing to host:port.
            let uri = generators::strip_route_uri_to_request_uri(first);
            let dest = dest_of(&strip_uri(&uri));
            // The worker no longer stamps `;outbound`: the front proxy double-
            // record-routes, so the worker-facing half of the dialog route set —
            // captured from the dialog-creating message (§12.1.1/§12.1.2) — is
            // ALREADY the proxy's own `;outbound` Record-Route on top. The proxy
            // reads direction from its own self-issued RR (registry- and pod-IP-
            // independent, so it survives a worker reboot), not from anything the
            // worker adds. We just forward the route set verbatim to the proxy.
            return (req, dest);
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
    let Some((host, port)) = config.b2b_outbound_proxy.clone() else {
        return (req, dest);
    };
    let route = MsgHeader {
        name: "Route".to_string(),
        value: format!("<sip:{host}:{port};lr>"),
    };
    req.headers.insert(0, route);
    (req, (host, port))
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
        if generators::first_route_is_loose(first) {
            let uri = generators::strip_route_uri_to_request_uri(first);
            return dest_of(&strip_uri(&uri));
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
#[allow(clippy::too_many_arguments)]
pub fn build_b_leg(
    call_ref: &str,
    leg_id: &str,
    // The call's emergency state (`call.emergency == Some(true)`); stamps the
    // `;em=1` / `;emerg=1` markers on the originated b-leg INVITE's Via + Contact
    // (port of `buildBLegInvite`, helpers.ts L214/L266-280). Every subsequent
    // in-dialog packet then carries it (the Tier-1 overload-brake signal).
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
    // Leg role (ADR-0014/0016). `None` ⇒ [`LegKind::Destination`]. `adopted` is
    // left `None` so it derives from the kind (`is_adopted`): a `media` leg is
    // unadopted and thus gated out of the generic relay-to-peer fallback.
    kind: Option<call::LegKind>,
) -> (Leg, OutboundSipEffect) {
    let branch = id_gen.new_branch();
    let from_tag = id_gen.new_tag();
    let b_call_id = format!("{}-{}@{}", leg_id, id_gen.new_tag(), config.sip_local_ip);
    let request_uri = new_ruri.map(str::to_string).unwrap_or_else(|| a_leg_invite.uri.clone());
    let from_uri = new_from.map(str::to_string).unwrap_or_else(|| a_leg_invite.from.uri.clone());
    let to_uri = new_to.map(str::to_string).unwrap_or_else(|| a_leg_invite.to.uri.clone());
    let body = match body_override {
        Some(b) => b.to_vec(),
        None => a_leg_invite.body.clone(),
    };
    let content_type = if body.is_empty() {
        None
    } else {
        get_header(&a_leg_invite.headers, "content-type").map(str::to_string)
            .or_else(|| body_override.map(|_| "application/sdp".to_string()))
    };
    // `(name, Some(v))` sets, `(name, None)` removes. Removals never apply to
    // structural headers (the generator owns those); only extra sets ride here.
    let mut extra_headers: Vec<MsgHeader> = header_updates
        .iter()
        .filter_map(|(n, v)| {
            v.as_ref().map(|val| MsgHeader { name: n.clone(), value: val.clone() })
        })
        .collect();
    // Advertise accepted methods + understood extensions on the originated b-leg
    // INVITE so the callee can negotiate UPDATE/PRACK/etc. (RFC 3261 §20.5/§20.37,
    // RFC 3311 §5) and the 2xx/re-INVITE audit (§13.2.1) sees a capability set.
    // `Supported` is a *default*: when a `relayFirst18x` strategy is active,
    // `apply_supported_for_18x` runs after this and rewrites it from alice's value
    // (stripping `100rel` as the strategy dictates). Neither clobbers a
    // caller-supplied value from `header_updates`.
    if !extra_headers.iter().any(|h| h.name.eq_ignore_ascii_case("Allow")) {
        extra_headers
            .push(MsgHeader { name: "Allow".to_string(), value: generators::B2BUA_ALLOW.to_string() });
    }
    if !extra_headers.iter().any(|h| h.name.eq_ignore_ascii_case("Supported")) {
        extra_headers.push(MsgHeader {
            name: "Supported".to_string(),
            value: generators::B2BUA_SUPPORTED.to_string(),
        });
    }

    // Opt-in transparent header relay (config.relay_headers, empty = no-op
    // default). Copy each named a-leg INVITE header verbatim onto this b-leg
    // INVITE. This single mint point covers BOTH originated legs: the normal
    // callee leg (apply_route) and the REFER transfer-target leg (actions.rs
    // passes `rebuild_a_leg_invite`, which rehydrates alice's full header
    // snapshot) — so one copy here reaches bob AND charlie. Guards: never
    // clobber a value already set via `header_updates` (case-insensitive), and
    // never relay a structural header the generator owns (so a misconfig can't
    // corrupt the dialog).
    const RELAY_FORBIDDEN: &[&str] = &[
        "via",
        "from",
        "to",
        "contact",
        "call-id",
        "cseq",
        "max-forwards",
        "route",
        "record-route",
        "content-length",
        "content-type",
    ];
    for name in &config.relay_headers {
        if extra_headers.iter().any(|h| h.name.eq_ignore_ascii_case(name)) {
            continue;
        }
        if RELAY_FORBIDDEN.iter().any(|f| name.eq_ignore_ascii_case(f)) {
            continue;
        }
        if let Some(v) = get_header(&a_leg_invite.headers, name) {
            extra_headers.push(MsgHeader { name: name.clone(), value: v.to_string() });
        }
    }

    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: request_uri.clone(),
        call_id: b_call_id.clone(),
        from_uri: from_uri.clone(),
        from_tag: from_tag.clone(),
        to_uri: to_uri.clone(),
        to_tag: None,
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
                original_invite: sip_message::serialize(&SipMessage::Request(invite.clone())),
                destination: call::HostPort {
                    host: wire_dest.0.clone(),
                    port: wire_dest.1,
                },
            }),
            cached_sdp: None,
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
    (leg, effect)
}

/// Headers a B2BUA must carry transparently when relaying an INVITE response
/// from the b-leg to the a-leg so end-to-end reliable-provisional (RFC 3262)
/// keeps working: `Require`/`Supported` (the `100rel` option-tag negotiation)
/// and `RSeq` (the provisional sequence the caller's PRACK acknowledges).
///
/// This is plain transparent relay — distinct from the deferred B2BUA-side
/// 18x-management *policies* (`relayFirst18xTo180`/`promote18xPemTo200`), which
/// would instead *rewrite* these provisionals.
pub fn relay_response_passthrough_headers(resp: &sip_message::SipResponse) -> Vec<MsgHeader> {
    const PASSTHROUGH: [&str; 3] = ["require", "rseq", "supported"];
    resp.headers
        .iter()
        .filter(|h| PASSTHROUGH.contains(&h.name.to_ascii_lowercase().as_str()))
        .map(|h| MsgHeader {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect()
}

/// Ensure an a-facing INVITE 2xx header set carries exactly ONE `Allow` and ONE
/// `Supported` — the B2BUA's own capability set (RFC 3261 §13.2.1/§20.37). The
/// B2BUA is a back-to-back UA: a 2xx it mints toward the caller must advertise
/// *its* methods/extensions, not whatever the callee's 200 happened to carry
/// (or omit). For each of Allow/Supported, `rule_stamped` names the headers the
/// firing rule already set with its own value (e.g. `promote18xPemTo200`
/// advertises `Supported` WITHOUT `100rel`, since Alice never saw a reliable
/// provisional from us) — those are kept verbatim and only de-duplicated. For
/// the rest we drop the callee's passed-through value and append the B2BUA
/// default. Either way exactly one of each results (no §7.3.1 duplicate);
/// `Require`/`RSeq` (reliable-provisional negotiation) are untouched.
pub fn stamp_a_facing_invite_advert(
    headers: &mut Vec<MsgHeader>,
    rule_stamped: &[(&'static str, String)],
) {
    let stamped = |name: &str| rule_stamped.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    for (name, default) in
        [("Allow", generators::B2BUA_ALLOW), ("Supported", generators::B2BUA_SUPPORTED)]
    {
        if stamped(name) {
            // The rule owns this value; just collapse any duplicate to one.
            let mut seen = false;
            headers.retain(|h| {
                if h.name.eq_ignore_ascii_case(name) {
                    let keep = !seen;
                    seen = true;
                    keep
                } else {
                    true
                }
            });
            continue;
        }
        // Replace any passed-through value with the B2BUA default, exactly once.
        headers.retain(|h| !h.name.eq_ignore_ascii_case(name));
        headers.push(MsgHeader { name: name.to_string(), value: default.to_string() });
    }
}

/// Headers carried transparently when relaying an in-dialog *request* across
/// the back-to-back UA (RFC 3261 §16.6 spirit — non-structural headers pass
/// through; structural ones are owned by the generator). For the basic-B2BUA
/// set this is the reliable-provisional negotiation (`Require`/`Supported`),
/// plus the payload-bearing headers of a **transparently relayed REFER/NOTIFY**
/// (newkahneed-019): a relayed REFER without its `Refer-To`/`Referred-By` is
/// malformed, and a relayed NOTIFY's implicit-subscription state lives in
/// `Event`/`Subscription-State` (the body carries only the sipfrag). These names
/// only occur on REFER/NOTIFY, so an OPTIONS/INFO/UPDATE relay is unaffected.
/// `RAck` is rewritten separately (RFC 3262 §7.2), not copied verbatim; the
/// generator adds `Event`/`Subscription-State` from its own opts ONLY for a
/// B2BUA-originated NOTIFY (`opts.event`/`subscription_state`), which the relay
/// path leaves unset — so these pass through here without duplicating.
pub fn relay_request_passthrough_headers(req: &SipRequest) -> Vec<MsgHeader> {
    const PASSTHROUGH: [&str; 6] = [
        "require",
        "supported",
        "refer-to",
        "referred-by",
        "event",
        "subscription-state",
    ];
    req.headers
        .iter()
        .filter(|h| PASSTHROUGH.contains(&h.name.to_ascii_lowercase().as_str()))
        .map(|h| MsgHeader {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect()
}

/// Build a UAS response on a leg's inbound INVITE (toward alice). `to_tag` pins
/// the stable a-facing dialog tag.
#[allow(clippy::too_many_arguments)]
pub fn response_to_a_leg(
    a_leg_invite: &SipRequest,
    status: u16,
    reason: &str,
    to_tag: Option<String>,
    contact: Option<ContactSpec>,
    body: Vec<u8>,
    content_type: Option<String>,
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
    // (top Via sent-by of her INVITE).
    let dest = top_via_host_port(a_leg_invite).unwrap_or_else(|| ("127.0.0.1".into(), 5060));
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
    content_type: Option<String>,
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
    let dest = dest_of(&strip_uri(&dialog.sip.remote_target));
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
        SipMessage::Request(r) => Some(r.cseq.seq),
        _ => None,
    }
}

/// The address a response to `req` is sent to (its topmost Via sent-by).
fn top_via_host_port(req: &SipRequest) -> Option<(String, u16)> {
    let via = get_header(&req.headers, "via")?;
    let after_transport = via.split_whitespace().nth(1)?;
    let sent_by = after_transport.split(';').next()?.trim();
    Some(dest_of(sent_by))
}

/// Reduce a SIP URI (`sip:user@host:port;params`) to its `host:port`.
pub fn strip_uri(uri: &str) -> String {
    let no_scheme = uri.strip_prefix("sips:").or_else(|| uri.strip_prefix("sip:")).unwrap_or(uri);
    let host_part = no_scheme.rsplit('@').next().unwrap_or(no_scheme);
    host_part.split([';', '?', '>']).next().unwrap_or(host_part).trim().to_string()
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    use sip_message::message_helpers::parse_uri_params;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
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
        let top_route = get_header(&out.headers, "route").expect("route header");
        // The top Route is unchanged (the proxy issued the `;outbound`, not us).
        assert_eq!(top_route, route, "egress must forward the captured route set verbatim");
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
        let top_route = get_header(&out.headers, "route").expect("route header");
        assert!(
            !parse_uri_params(top_route).contains_key("outbound"),
            "egress must not stamp ;outbound; got {top_route}"
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
        let top_route = get_header(&out.headers, "route").expect("preloaded route");
        assert_eq!(top_route, "<sip:10.0.0.9:5060;lr>", "bootstrap preload must be a plain loose Route");
        assert!(!parse_uri_params(top_route).contains_key("outbound"), "no ;outbound on the bootstrap preload");
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
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

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

    /// The Contact user from the b-leg INVITE's first Contact URI.
    fn contact_user(req: &SipRequest) -> String {
        let c = get_header(&req.headers, "contact").expect("b-leg INVITE has a Contact");
        // `<sip:user@host:port;params>` → user
        let inside = c.trim_start_matches('<').trim_end_matches('>');
        let no_scheme = inside.strip_prefix("sip:").or_else(|| inside.strip_prefix("sips:")).unwrap_or(inside);
        no_scheme.split('@').next().unwrap_or("").to_string()
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
            None, // Destination leg
        );

        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };

        // (a) ID-1 — fresh Call-ID, NOT the a-leg's.
        assert_ne!(invite.call_id, a.call_id, "b-leg Call-ID must not be the a-leg's");
        assert_eq!(invite.call_id, leg.call_id, "leg + INVITE Call-IDs agree");
        // The mint shape is `<leg>-<tag>@<local_ip>` (relay.rs), so it carries the
        // leg id and the B2BUA's own host — never alice's Call-ID host.
        assert!(invite.call_id.starts_with("b-1-"), "Call-ID carries the leg id: {}", invite.call_id);
        assert!(invite.call_id.ends_with("@127.0.0.1"), "Call-ID host is the B2BUA's: {}", invite.call_id);

        // (b) ID-2 — fresh From-tag (B2BUA-owned), NOT the a-leg's; To-tag absent
        // on the initial INVITE (the callee mints it in its 2xx, RFC 3261 §12.1.1).
        let from_tag = invite.from.tag.as_deref().expect("b-leg From carries a tag");
        assert_ne!(from_tag, "alice-from-tag", "b-leg From-tag must not be the a-leg's");
        assert_eq!(from_tag, leg.from_tag, "leg + INVITE From-tags agree");
        assert!(invite.to.tag.is_none(), "initial b-leg INVITE has no To-tag, got {:?}", invite.to.tag);

        // (c) ID-3 — the b-leg dialog starts a fresh CSeq space at 1 (the a-leg's
        // INVITE was CSeq 314).
        assert_eq!(invite.cseq.seq, 1, "b-leg CSeq starts at 1, independent of the a-leg's 314");

        // (d) HDR-2 — Contact is the B2BUA's own address (host = local_ip), and its
        // user is the B2BUA's, NOT the a-leg caller's ("alice").
        let cuser = contact_user(&invite);
        assert_ne!(cuser, "alice", "b-leg Contact user must not be the a-leg caller's");
        assert_eq!(cuser, "b2bua", "b-leg Contact is the B2BUA's own identity");
        let contact = get_header(&invite.headers, "contact").unwrap();
        assert!(
            contact.contains("127.0.0.1"),
            "b-leg Contact host must be the B2BUA local addr: {contact}"
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
            None,
        );
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        (
            get_header(&invite.headers, "via").expect("b-leg INVITE has a Via").to_string(),
            get_header(&invite.headers, "contact").expect("b-leg INVITE has a Contact").to_string(),
        )
    }

    // The wiring contract this slice closes: an EMERGENCY call's initial b-leg
    // INVITE (the single mint point) carries `;em=1` on its Via and `;emerg=1`
    // on its Contact ON THE WIRE — the in-dialog markers `buffer_has_emergency_
    // marker` scans so the Tier-1 overload brake never 503s an admitted
    // emergency call. The pure-builder tests prove the `if is_emergency` branch
    // in isolation; this proves a production relay path actually passes `true`
    // and the markers reach the serialized message (port of `buildBLegInvite`,
    // helpers.ts L266-280).
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

    /// An a-leg INVITE carrying a relayable correlation header (`X-Loadgen-Id`)
    /// plus a `To` (a structural header that must NEVER be relayed even if named).
    fn a_leg_invite_with_relay_header() -> SipRequest {
        parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
X-Loadgen-Id: lg-abc123\r\n\
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
            None,
        );
        match effect.body {
            OutboundBody::Request(r) => r.headers,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        }
    }

    /// Opt-in transparent relay: a named a-leg header rides onto BOTH originated
    /// legs (callee + REFER transfer target), the empty default is a strict no-op,
    /// and a structural header named in the relay list is NEVER duplicated. Both
    /// legs are asserted at the single `build_b_leg` mint point — charlie's leg
    /// goes through the exact same fn with the rebuilt a-leg invite, so a second
    /// call with a charlie R-URI faithfully simulates the REFER transfer leg.
    #[test]
    fn relay_headers_copy_to_both_legs_and_never_clobber_structural() {
        let has = |hs: &[MsgHeader], name: &str, val: &str| {
            hs.iter().any(|h| h.name.eq_ignore_ascii_case(name) && h.value == val)
        };

        // (a) NORMAL callee leg (bob): the named header is relayed verbatim.
        let bob = relay_b_leg_headers(vec!["X-Loadgen-Id".into()], None);
        assert!(
            has(&bob, "X-Loadgen-Id", "lg-abc123"),
            "callee leg must carry the relayed X-Loadgen-Id: {bob:?}"
        );

        // (b) REFER transfer leg (charlie): same mint point, R-URI re-aimed → the
        // header still rides. Proves the single insertion point covers charlie.
        let charlie = relay_b_leg_headers(
            vec!["X-Loadgen-Id".into()],
            Some("sip:charlie@10.244.2.9:5060"),
        );
        assert!(
            has(&charlie, "X-Loadgen-Id", "lg-abc123"),
            "REFER transfer leg must carry the relayed X-Loadgen-Id: {charlie:?}"
        );

        // (c) Default (empty list) is a strict no-op: the header is ABSENT.
        let noop = relay_b_leg_headers(Vec::new(), None);
        assert!(
            !noop.iter().any(|h| h.name.eq_ignore_ascii_case("X-Loadgen-Id")),
            "empty relay_headers must NOT copy the header: {noop:?}"
        );

        // (d) A structural header named in the list is NEVER relayed: the b-leg To
        // is minted structurally by the generator (the generator owns it), so the
        // relay path must skip it and NOT push a second copy. Naming "To" in the
        // relay list yields exactly ONE To header — the structural one — proving
        // the forbidden-set guard blocks the relay duplication that a misconfig
        // would otherwise introduce.
        let with_to = relay_b_leg_headers(
            vec!["X-Loadgen-Id".into(), "To".into()],
            None,
        );
        let to_count = with_to.iter().filter(|h| h.name.eq_ignore_ascii_case("To")).count();
        assert_eq!(to_count, 1, "exactly one structural To header, never a relayed dup: {with_to:?}");
        // The relayable header still rode alongside the rejected structural one.
        assert!(has(&with_to, "X-Loadgen-Id", "lg-abc123"));
    }
}
