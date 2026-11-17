//! A response to a request of ours for a call this node does not hold,
//! answered as an unknown dialog would be (ADR-0055 §4) when its via-extension
//! verifies (ADR-0055 §12): a 2xx to an INVITE is ACKed and its dialog BYEd
//! from the 2xx's own headers (RFC 3261 §13.2.2.4, §15.1.1), on this node's
//! Via whichever node sent the INVITE (§8.1.1.7), an offer the 2xx
//! makes to an INVITE that carried none answered with every stream rejected
//! (RFC 3264 §4, §6); a non-2xx final to an INVITE is ACKed hop by hop
//! (§17.1.1.3) where the INVITE went (§17.1.1.2), on the Request-URI and the
//! route the extension states. A BYE into a dialog already ended draws a 481 and
//! does no harm. Anything else is owed nothing.
//!
//! The answer is drawn per copy of the final and keyed by it ([`UnheldKey`]):
//! the shell's book sends what a copy is owed (ADR-0063 §2), the ACK on the
//! final's ladder ([`crate::copy_schedule`]), the BYE once.

use sip_ids::TurnIds;
use sip_message::generators::{
    generate_ack_for_unheld_2xx, generate_ack_for_unheld_non_2xx, generate_bye_for_unheld_2xx,
};
use sip_message::header::Uri;
use sip_message::{Method, SipRequest, SipResponse, SipStr};
use sip_retransmit::Class;

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance};
use crate::rules::relay::{apply_b_leg_egress, target_dest};
use crate::stack_identity::{call_via_of, read_via_extension};

/// The final an unheld answer answers: its dialog's Call-ID and To-tag and
/// the number of the INVITE it answers. Every copy of one final has one key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnheldKey {
    pub call_id: String,
    pub to_tag: String,
    pub cseq: u32,
}

/// What one copy of an INVITE final no call here holds draws, before the
/// book bounds it: the ACK, and for a 2xx the BYE of its dialog.
#[derive(Clone, Debug)]
pub struct UnheldAnswer {
    pub key: UnheldKey,
    /// The ladder the final's answerer re-sends it on.
    pub class: Class,
    pub ack: OutboundSipEffect,
    pub bye: Option<OutboundSipEffect>,
}

impl UnheldAnswer {
    /// Every message the answer holds, the ACK first.
    pub fn into_effects(self) -> Vec<OutboundSipEffect> {
        std::iter::once(self.ack).chain(self.bye).collect()
    }

    /// The answer with each message naming `leg_id`.
    pub fn on_leg(mut self, leg_id: &str) -> Self {
        for eff in std::iter::once(&mut self.ack).chain(self.bye.as_mut()) {
            eff.leg_id = Some(leg_id.to_string());
        }
        self
    }
}

/// What this node owes `resp`, a response to a request of ours whose call it
/// does not hold, received from `src` at `now_ms`; `None` when it owes
/// nothing. `ids` draws the branches.
pub fn answer_unheld_response(
    config: &B2buaConfig,
    resp: &SipResponse,
    src: (String, u16),
    ids: &mut TurnIds,
    now_ms: i64,
) -> Option<UnheldAnswer> {
    if resp.cseq().method() != Method::Invite || resp.status() < 200 {
        return None;
    }
    let ext = read_via_extension(resp.top_via(), "INVITE", config.cluster_key.as_ref())?;
    let key = UnheldKey {
        call_id: resp.call_id().as_str().to_string(),
        to_tag: resp.to().tag().unwrap_or_default().to_string(),
        cseq: resp.cseq().seq(),
    };
    let class = crate::copy_schedule::class_of(resp.status());
    if resp.status() < 300 {
        let answer = (!ext.offer).then(|| rejecting_answer(config, resp, now_ms)).flatten();
        let (ack, bye) = answer_2xx(config, resp, &ext.leg, answer, ids)?;
        return Some(UnheldAnswer { key, class, ack, bye: Some(bye) });
    }
    let uri = Uri::parse(&SipStr::owned(&ext.request_uri)).ok()?;
    let ack = generate_ack_for_unheld_non_2xx(resp, uri, &ext.route).ok()?;
    // The front proxy's first hop, whichever leg; with none, the request's own.
    let first_hop = if config.front.outbound_proxy().is_some() { src } else { next_hop(&ack) };
    let (ack, dest) = apply_b_leg_egress(config, &ext.leg, &[], ack, first_hop);
    let ack = effect(ack, dest, OutboundTxnMode::Raw, "ACK (non-2xx, unheld)", &ext.leg);
    Some(UnheldAnswer { key, class, ack, bye: None })
}

/// Where `req` goes with no front proxy: its first Route, else its Request-URI
/// (RFC 3261 §8.1.2).
fn next_hop(req: &SipRequest) -> (String, u16) {
    let routes = req.route_set().ok();
    match routes.as_ref().and_then(|routes| routes.first()) {
        Some(route) => {
            let (host, port) = route.uri().next_hop();
            (host.to_string(), port)
        }
        None => target_dest(&req.request_uri().to_string()),
    }
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
) -> Option<(OutboundSipEffect, OutboundSipEffect)> {
    let next = resp.cseq().seq().saturating_add(1);
    let hop = |branch: String| {
        call_via_of(resp.top_via(), &config.sip_local_ip, config.sip_local_port, branch)
    };
    let (ack_hop, bye_hop) = (hop(ids.new_branch())?, hop(ids.new_branch())?);
    let ack = generate_ack_for_unheld_2xx(resp, ack_hop, answer).ok()?;
    let bye = generate_bye_for_unheld_2xx(resp, next, bye_hop).ok()?;
    let route_set: Vec<String> =
        ack.raw_text(sip_message::header::HeaderName::Route).map(|r| r.to_string()).collect();
    let dest = target_dest(&ack.request_uri().to_string());
    let (ack, ack_dest) = apply_b_leg_egress(config, leg, &route_set, ack, dest.clone());
    let (bye, bye_dest) = apply_b_leg_egress(config, leg, &route_set, bye, dest);
    Some((
        effect(ack, ack_dest, OutboundTxnMode::Raw, "ACK (2xx, unheld)", leg),
        effect(
            bye,
            bye_dest,
            OutboundTxnMode::NewClient(call::TxnKind::NonInvite),
            "BYE (2xx, unheld)",
            leg,
        ),
    ))
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
