//! One parsed message taken into its leg: the correlation tokens it carries,
//! the leg's INVITE summary, first final and teardown, the retransmission
//! mark, and the hop it was observed at.

use std::net::SocketAddr;

use sip_message::header::{HeaderName, Params};
use sip_message::{Method, SipMessage};

use super::{CorrelateStrategy, FlowLeg, FlowMsg, Hop, InviteSummary, Terminator};

pub(super) fn ingest(
    leg: &mut FlowLeg,
    ts_us: u64,
    src: SocketAddr,
    dst: SocketAddr,
    msg: SipMessage,
    probe: u32,
    strategies: &[CorrelateStrategy],
) {
    for (si, strat) in strategies.iter().enumerate() {
        match strat {
            CorrelateStrategy::HeaderToken { headers } => {
                for name in headers {
                    for v in msg.raw(HeaderName::from(name.as_str())) {
                        let v = v.trim();
                        if !v.is_empty() {
                            leg.tokens_by_strategy[si].insert(v.to_string());
                        }
                    }
                }
            }
            CorrelateStrategy::HeaderParam { header, param } => {
                for v in msg.raw_text(HeaderName::from(header.as_str())) {
                    let params = Params::parse_list(&v);
                    if let Some(t) = params.value(param).filter(|t| !t.is_empty()) {
                        leg.tokens_by_strategy[si].insert(t.to_string());
                    }
                }
            }
            CorrelateStrategy::DerivedCallId { .. }
            | CorrelateStrategy::IdentityAdjacency { .. } => {}
        }
    }
    match &msg {
        SipMessage::Request(r) => {
            if r.method() == Method::Invite && leg.invite.is_none() {
                leg.invite = Some(InviteSummary {
                    ruri: r.request_uri().clone(),
                    from_uri: r.from().uri().clone(),
                    to_uri: r.to().uri().clone(),
                    cseq: r.cseq().seq(),
                });
            }
            if r.method() == Method::Bye && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Bye);
            }
            if r.method() == Method::Cancel && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Cancel);
            }
        }
        SipMessage::Response(r) => {
            if r.cseq().method() == Method::Invite {
                if r.status() == 180 {
                    leg.saw_180 = true;
                }
                let initial = leg.invite.as_ref().map(|inv| inv.cseq);
                if r.status() >= 200
                    && leg.final_status.is_none()
                    && (initial.is_none() || initial == Some(r.cseq().seq()))
                {
                    leg.final_status = Some(r.status());
                }
            }
        }
    }
    // Retransmission tag: the SAME DATAGRAM already seen in this direction on
    // this leg, inside the transaction envelope. The EARLIEST match anchors it,
    // because 64·T1 runs from the first emission — see [`crate::callfacts`],
    // which decides the same relation for the emitted document and must not be
    // able to disagree with this one.
    let retx = leg
        .msgs
        .iter()
        .find(|m| m.src == src && m.dst == dst && m.parsed.image() == msg.image())
        .is_some_and(|m| ts_us.saturating_sub(m.ts_us) <= crate::callfacts::REPEAT_ENVELOPE_US);
    let pair = Hop::normalized(src, dst);
    let hop = match leg.hops.iter().position(|h| *h == pair) {
        Some(h) => h,
        None => {
            leg.hops.push(pair);
            leg.hops.len() - 1
        }
    };
    leg.msgs.push(FlowMsg { ts_us, src, dst, parsed: msg, hop, retx, probe });
}
