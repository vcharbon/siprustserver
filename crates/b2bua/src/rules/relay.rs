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

/// Build a fresh b-leg + its outbound INVITE effect (initial route + failover).
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
) -> (Leg, OutboundSipEffect) {
    let branch = id_gen.new_branch();
    let from_tag = id_gen.new_tag();
    let b_call_id = format!("{}-{}@{}", leg_id, id_gen.new_tag(), config.sip_local_ip);
    let request_uri = new_ruri.map(str::to_string).unwrap_or_else(|| a_leg_invite.uri.clone());
    let from_uri = a_leg_invite.from.uri.clone();
    let to_uri = a_leg_invite.to.uri.clone();
    let content_type = get_header(&a_leg_invite.headers, "content-type").map(str::to_string);

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
        body: a_leg_invite.body.clone(),
        content_type,
        extra_headers: vec![],
    };
    let invite = generators::generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);

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
                    host: dest.0.clone(),
                    port: dest.1,
                },
            }),
            cached_sdp: None,
        },
    };

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
        pending_invite_txn: None,
        ext: None,
        kind: Some(call::LegKind::Destination),
        adopted: Some(false),
    };

    let effect = OutboundSipEffect {
        body: OutboundBody::Request(invite),
        mode: OutboundTxnMode::NewClient(TxnKind::Invite),
        destination: dest,
        label: format!("b-leg INVITE ({leg_id})"),
        leg_id: Some(leg_id.to_string()),
    };
    (leg, effect)
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
) -> OutboundSipEffect {
    let opts = GenerateResponseOpts {
        to_tag,
        contact,
        body,
        content_type,
        extra_headers: vec![],
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

/// Build an ACK-for-2xx on a b-leg dialog (toward bob), sent raw.
pub fn ack_b_leg(call_ref: &str, leg: &Leg, config: &B2buaConfig, id_gen: &IdGen) -> Option<OutboundSipEffect> {
    let dialog = leg.dialogs.first()?;
    let gen_dialog = to_gen_dialog(&dialog.sip);
    let branch = id_gen.new_branch();
    let opts = GenerateAckFor2xxOpts {
        via: Some(leg_via(config, call_ref, &leg.leg_id, branch)),
        cseq: Some(dialog.sip.local_cseq.max(0) as u32),
        ..Default::default()
    };
    let ack = generators::generate_ack_for_2xx(None, &gen_dialog, &opts);
    let dest = dest_of(&strip_uri(&dialog.sip.remote_target));
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
