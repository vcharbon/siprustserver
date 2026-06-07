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
pub fn rebuild_a_leg_invite(call: &call::Call) -> SipRequest {
    let snap = &call.a_leg_invite;
    hydrate_request(
        "INVITE",
        &snap.uri,
        to_msg_headers(&snap.headers),
        snap.body.clone(),
    )
    .expect("a-leg INVITE snapshot is well-formed")
}

/// The B2BUA's Via for a leg's outbound message.
pub fn leg_via(config: &B2buaConfig, call_ref: &str, leg_id: &str, branch: String) -> ViaSpec {
    build_call_via(
        &StackIdentityOpts {
            local_ip: &config.sip_local_ip,
            local_port: config.sip_local_port,
            call_ref,
            leg: leg_id,
            is_emergency: false,
        },
        branch,
    )
}

/// The B2BUA's Contact for a leg's outbound message.
pub fn leg_contact(config: &B2buaConfig, call_ref: &str, leg_id: &str) -> ContactSpec {
    build_call_contact(&StackIdentityOpts {
        local_ip: &config.sip_local_ip,
        local_port: config.sip_local_port,
        call_ref,
        leg: leg_id,
        is_emergency: false,
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

/// Build a fresh b-leg + its outbound INVITE effect (initial route + failover).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn build_b_leg(
    call_ref: &str,
    leg_id: &str,
    a_leg_invite: &SipRequest,
    dest: (String, u16),
    new_ruri: Option<&str>,
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
    let from_uri = a_leg_invite.from.uri.clone();
    let to_uri = a_leg_invite.to.uri.clone();
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
    let extra_headers: Vec<MsgHeader> = header_updates
        .iter()
        .filter_map(|(n, v)| {
            v.as_ref().map(|val| MsgHeader { name: n.clone(), value: val.clone() })
        })
        .collect();

    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: request_uri.clone(),
        call_id: b_call_id.clone(),
        from_uri: from_uri.clone(),
        from_tag: from_tag.clone(),
        to_uri: to_uri.clone(),
        to_tag: None,
        cseq: 1,
        via: Some(leg_via(config, call_ref, leg_id, branch.clone())),
        contact: Some(leg_contact(config, call_ref, leg_id)),
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

/// Headers carried transparently when relaying an in-dialog *request* across
/// the back-to-back UA (RFC 3261 §16.6 spirit — non-structural headers pass
/// through; structural ones are owned by the generator). For the basic-B2BUA
/// set this is the reliable-provisional negotiation (`Require`/`Supported`);
/// `RAck` is rewritten separately (RFC 3262 §7.2), not copied verbatim.
pub fn relay_request_passthrough_headers(req: &SipRequest) -> Vec<MsgHeader> {
    const PASSTHROUGH: [&str; 2] = ["require", "supported"];
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
pub fn ack_b_leg(
    call_ref: &str,
    leg: &Leg,
    config: &B2buaConfig,
    id_gen: &IdGen,
    body: Vec<u8>,
    content_type: Option<String>,
) -> Option<OutboundSipEffect> {
    let dialog = leg.dialogs.first()?;
    let gen_dialog = to_gen_dialog(&dialog.sip);
    let branch = id_gen.new_branch();
    let opts = GenerateAckFor2xxOpts {
        via: Some(leg_via(config, call_ref, &leg.leg_id, branch)),
        cseq: Some(dialog.sip.local_cseq.max(0) as u32),
        body,
        content_type,
        ..Default::default()
    };
    let ack = generators::generate_ack_for_2xx(None, &gen_dialog, &opts);
    let dest = dest_of(&strip_uri(&dialog.sip.remote_target));
    let (ack, dest) = apply_b_leg_egress(config, &leg.leg_id, &gen_dialog.route_set, ack, dest);
    Some(OutboundSipEffect {
        body: OutboundBody::Request(ack),
        mode: OutboundTxnMode::Raw,
        destination: dest,
        label: format!("ACK → {}", leg.leg_id),
        leg_id: Some(leg.leg_id.clone()),
    })
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
}
