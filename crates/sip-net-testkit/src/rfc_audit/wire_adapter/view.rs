//! One bind's stream in the wire model: the audit-visible events projected
//! per bind, and the closed observation over each.

use std::collections::HashMap;

use layer_harness::{LaneKey, Stamped};
use sip_message::{SipMessage, SipParser};

use rfc_rules::{Kind, Msg, Observation};

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::msg_reads::{
    call_id, cseq_method, cseq_seq, from_tag, status, to_tag, top_via_branch,
};

/// One bind's stream in the wire model, with the per-message bookkeeping the
/// adapter needs to format and anchor a finding.
pub(super) struct BindView {
    pub(super) bind: LaneKey,
    pub(super) msgs: Vec<Msg>,
    /// Per msg: the originating event's stamp `seq` (the waiver anchor) and
    /// the Call-ID (finding text; the wire model itself never keys on it).
    pub(super) meta: Vec<(u64, String)>,
}

/// Project the (audit-visible) events into per-bind views, in event order.
pub(super) fn bind_views(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<BindView> {
    let mut order: Vec<LaneKey> = Vec::new();
    let mut views: HashMap<LaneKey, BindView> = HashMap::new();
    let mut send_tables: HashMap<LaneKey, crate::repeat::RepeatTables> = HashMap::new();
    // The repeat chains (keyed by their first sighting's seq) whose first
    // copy this view holds. A chain whose first sighting is not in the view
    // (modeled loss) starts at the first copy that is: the endpoint took it
    // as new.
    let mut held_chains: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let parser = crate::rfc_audit::lenient_parser();

    for s in events {
        let (bind, parsed, repeat, src, dst, head): (
            &LaneKey,
            SipMessage,
            bool,
            String,
            String,
            Vec<u8>,
        ) = match &s.event {
            SignalingNetworkEvent::RecvItem { bind_key, packet, wire, .. } => {
                // Parsed once at event production; a datagram even the lenient
                // parser rejects can key no obligation.
                let Some(parsed) = wire.parsed.as_deref() else { continue };
                let chain = wire.repeat_of.unwrap_or(s.seq);
                (
                    bind_key,
                    parsed.clone(),
                    !held_chains.insert(chain),
                    packet.src.to_string(),
                    bind_key.clone(),
                    packet.raw.to_vec(),
                )
            }
            SignalingNetworkEvent::SendCalled { bind_key, to, msg } => {
                let Ok(parsed) = parser.parse(msg) else { continue };
                let repeat = send_tables
                    .entry(bind_key.clone())
                    .or_default()
                    .note(msg, &parsed, s.seq)
                    .is_some();
                (bind_key, parsed, repeat, bind_key.clone(), to.to_string(), msg.clone())
            }
            _ => continue,
        };
        let kind = match &parsed {
            SipMessage::Request(r) => Kind::Request { method: r.method().to_string() },
            SipMessage::Response(_) => Kind::Response { status: status(&parsed) },
        };
        let view = views.entry(bind.clone()).or_insert_with(|| {
            order.push(bind.clone());
            BindView { bind: bind.clone(), msgs: Vec::new(), meta: Vec::new() }
        });
        // Strictly monotonic per-view stamps: the channel records at ms
        // resolution, so a relay's receive and forward can TIE — which would
        // defeat the rules' order-based hop gates (who emitted first, who
        // passed a message on). Event `seq` is the total order (the wire
        // model's tiebreak); a µs bump inside the tied ms preserves it.
        let at_us = (s.at_ms * 1_000).max(view.msgs.last().map_or(0, |m| m.at_us + 1));
        view.msgs.push(Msg {
            at_us,
            src,
            dst,
            hop: 0,
            repeat,
            kind,
            call_id: call_id(&parsed).to_string(),
            cseq: cseq_seq(&parsed),
            cseq_method: cseq_method(&parsed).to_string(),
            via_branch: top_via_branch(&parsed),
            from_tag: from_tag(&parsed).map(str::to_string),
            to_tag: to_tag(&parsed).map(str::to_string),
            head: Some(head),
            // The vantage IS the datagram, so the body is always carried: a
            // message with none states an EMPTY one, never an unknown one.
            body: Some(body_of(&parsed).to_vec()),
        });
        view.meta.push((s.seq, call_id(&parsed).to_string()));
    }
    order.into_iter().filter_map(|b| views.remove(&b)).collect()
}

/// The body bytes a parsed message carried — empty where it carried none.
fn body_of(m: &SipMessage) -> &[u8] {
    match m {
        SipMessage::Request(r) => r.body(),
        SipMessage::Response(r) => r.body(),
    }
}

/// The CLOSED observation over one bind's stream.
pub(super) fn observation(msgs: &[Msg]) -> Observation {
    let mut obs = Observation { closed: true, ..Observation::default() };
    for m in msgs {
        obs.last_us = obs.last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = obs.endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    obs
}
