//! The client INVITE transactions a materialised call still owes an ACK on:
//! a leg's dialling INVITE that took a non-2xx final, within Timer D of that
//! final's arrival. The transaction layer holds that ACK until the turn that
//! took the final released it (ADR-0055 §3), so the node that died may never
//! have sent it, and the callee repeats its final until one arrives (RFC 3261
//! §17.2.1).

use call::{Call, Leg};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_txn::TxnSeed;

/// One seed per leg of `call` whose dialling INVITE took a non-2xx final
/// less than Timer D before `now_ms`: Completed for what is left of it.
pub(in crate::router) fn rejected_dialled_seeds(call: &Call, now_ms: i64) -> Vec<TxnSeed> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .filter_map(|leg| seed(leg, now_ms))
        .collect()
}

fn seed(leg: &Leg, now_ms: i64) -> Option<TxnSeed> {
    let dialled = leg.outbound_invite()?;
    dialled.final_received.filter(|s| *s >= 300)?;
    let timer_d = i64::try_from(sip_txn::timers::TIMER_D).ok()?;
    let left = dialled.final_at_ms?.saturating_add(timer_d).saturating_sub(now_ms);
    let bound_ms = u64::try_from(left).ok().filter(|ms| *ms > 0)?;
    let sent = leg.dialled_sent()?;
    let invite = match CustomParser::new().parse(&sent.image).ok()? {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => return None,
    };
    let dest = sip_net::ip_socket_addr(&sent.destination.host, sent.destination.port)?;
    Some(TxnSeed::ClientRejected { invite, dest, bound_ms })
}
