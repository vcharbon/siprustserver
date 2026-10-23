//! A response to a request of ours for a call this node does not hold,
//! answered as an unknown dialog would be (ADR-0055 §4) when its via-extension
//! verifies (ADR-0055 §12): a 2xx to an INVITE is ACKed and its dialog BYEd
//! from the 2xx's own headers (RFC 3261 §13.2.2.4, §15.1.1), an offer the 2xx
//! makes to an INVITE that carried none answered with every stream rejected
//! (RFC 3264 §4, §6); a non-2xx final to an INVITE is ACKed hop by hop
//! (§17.1.1.3) at the Request-URI the extension states. A BYE into a dialog
//! already ended draws a 481 and does no harm. Anything else is owed nothing.

use sip_ids::TurnIds;
use sip_message::generators::{
    generate_ack_for_unheld_2xx, generate_ack_for_unheld_non_2xx, generate_bye_for_unheld_2xx,
};
use sip_message::header::Uri;
use sip_message::{Method, SipRequest, SipResponse, SipStr};

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance};
use crate::rules::relay::{apply_b_leg_egress, target_dest};
use crate::stack_identity::read_via_extension;

/// What this node sends for `resp`, a response to a request of ours whose
/// call it does not hold, received from `src` at `now_ms`. `ids` draws the
/// branches.
pub fn answer_unheld_response(
    config: &B2buaConfig,
    resp: &SipResponse,
    src: (String, u16),
    ids: &mut TurnIds,
    now_ms: i64,
) -> Vec<OutboundSipEffect> {
    if resp.cseq().method() != Method::Invite || resp.status() < 200 {
        return Vec::new();
    }
    let via = resp.top_via();
    let Some(ext) = read_via_extension(via, "INVITE", config.cluster_key.as_ref()) else {
        return Vec::new();
    };
    if resp.status() < 300 {
        let answer = (!ext.offer).then(|| rejecting_answer(config, resp, now_ms)).flatten();
        return answer_2xx(config, resp, &ext.leg, answer, ids);
    }
    let Ok(uri) = Uri::parse(&SipStr::owned(&ext.request_uri)) else { return Vec::new() };
    let Ok(ack) = generate_ack_for_unheld_non_2xx(resp, uri) else { return Vec::new() };
    let (ack, dest) = apply_b_leg_egress(config, &ext.leg, &[], ack, src);
    vec![effect(ack, dest, OutboundTxnMode::Raw, "ACK (non-2xx, unheld)", &ext.leg)]
}

/// The answer to the offer `resp` makes, every stream rejected (RFC 3264 §6).
fn rejecting_answer(config: &B2buaConfig, resp: &SipResponse, now_ms: i64) -> Option<Vec<u8>> {
    let options =
        sip_message::BuildHeldSdpOptions { local_ip: config.sip_local_ip.clone(), now_ms };
    sip_message::sdp_answer::reject_offer(resp.sdp()?, &options)
}

/// The ACK, carrying `answer`, and the BYE of a 2xx whose dialog no record
/// holds.
fn answer_2xx(
    config: &B2buaConfig,
    resp: &SipResponse,
    leg: &str,
    answer: Option<Vec<u8>>,
    ids: &mut TurnIds,
) -> Vec<OutboundSipEffect> {
    let next = resp.cseq().seq().saturating_add(1);
    let (Ok(ack), Ok(bye)) = (
        generate_ack_for_unheld_2xx(resp, &ids.new_branch(), answer),
        generate_bye_for_unheld_2xx(resp, next, &ids.new_branch()),
    ) else {
        return Vec::new();
    };
    let route_set: Vec<String> =
        ack.raw_text(sip_message::header::HeaderName::Route).map(|r| r.to_string()).collect();
    let dest = target_dest(&ack.request_uri().to_string());
    let (ack, ack_dest) = apply_b_leg_egress(config, leg, &route_set, ack, dest.clone());
    let (bye, bye_dest) = apply_b_leg_egress(config, leg, &route_set, bye, dest);
    vec![
        effect(ack, ack_dest, OutboundTxnMode::Raw, "ACK (2xx, unheld)", leg),
        effect(
            bye,
            bye_dest,
            OutboundTxnMode::NewClient(call::TxnKind::NonInvite),
            "BYE (2xx, unheld)",
            leg,
        ),
    ]
}

fn effect(
    req: SipRequest,
    destination: (String, u16),
    mode: OutboundTxnMode,
    label: &str,
    leg: &str,
) -> OutboundSipEffect {
    OutboundSipEffect {
        body: OutboundBody::Request(req),
        mode,
        destination,
        label: label.to_string(),
        leg_id: Some(leg.to_string()),
        provenance: Provenance::Authored,
    }
}
