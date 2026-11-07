//! RFC 3261 §18.2.2 — where a response over unreliable unicast goes: the
//! address its own top Via names.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use sip_message::header::Rport;
use sip_message::ip_literal::ip_host;
use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{endpoint_addr, Kind, WireView};

use super::super::Obligation;

/// **RFC 3261 §18.2.2, RFC 3581 §4 — a response over unreliable unicast goes
/// to the address its top Via names.** The host is `received` where the Via
/// carries one, the sent-by host otherwise; the port is an observed `rport`
/// where the Via carries one, the sent-by port otherwise (5060 by default). A
/// Via's `maddr` chooses no destination (ADR-0001). A response sent elsewhere
/// reaches no one the request came from: a NAT binding, or a socket that is not
/// listening.
///
/// **The occasion is ONE response over UDP whose top Via reads: a fresh one,
/// or a repeat sent somewhere its first copy was not.** The stamp is on the
/// response itself, so no request is paired. A reliable
/// transport answers on the request's connection, and a sent-by name is
/// resolved by RFC 3263 §5, which this view does not hold: the first opens no
/// occasion, the second is `Undecidable`. Charges the sender.
pub struct ResponseDestination;

impl Obligation for ResponseDestination {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Where each response first went, by sender and transaction: a repeat
        // to the same place is that copy again.
        let mut first: BTreeMap<(&str, &str, Option<&str>, u32, u16), &str> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Kind::Response { status } = msg.kind else { continue };
            let key = (
                msg.src.as_str(),
                msg.call_id.as_str(),
                msg.via_branch.as_deref(),
                msg.cseq,
                status,
            );
            let went = *first.entry(key).or_insert(msg.dst.as_str());
            if msg.repeat && went == msg.dst {
                continue;
            }
            let finding = |decision| Finding {
                rule: RuleId::ResponseDestination,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let Some(via) = sniff::top_via(head) else {
                out.push(finding(Decision::Undecidable("no top Via a reader accepts")));
                continue;
            };
            if !via.transport().eq_ignore_ascii_case("UDP") {
                continue;
            }
            let (host, port) = via.response_target();
            let Some(ip) = ip_host(host) else {
                out.push(finding(Decision::Undecidable("a sent-by name RFC 3263 §5 resolves")));
                continue;
            };
            let Some(sent_to) = endpoint_addr(&msg.dst) else {
                out.push(finding(Decision::Undecidable("no wire destination at this vantage")));
                continue;
            };
            let target = SocketAddr::new(ip.to_canonical(), port);
            let sent_to = SocketAddr::new(sent_to.ip().to_canonical(), sent_to.port());
            if sent_to == target {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::ResponseDestinationDiverged {
                response_dest_msg: mi,
                response_dest_hop: msg.hop,
                response_dest_ts_us: msg.at_us,
                status,
                response_dest_method: msg.cseq_method.clone(),
                dest_sent_to: sent_to.to_string(),
                via_target: target.to_string(),
                rport_unfilled: matches!(via.rport(), Some(Rport::Requested)),
            })));
        }
        out
    }
}

#[cfg(test)]
mod tests;
