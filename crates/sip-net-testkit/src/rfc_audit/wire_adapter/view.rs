//! One bind's stream in the wire model: the audit-visible events projected
//! per bind, and the closed observation over each. A datagram the audit's
//! lenient reading refuses is projected off its identity lines.

use std::collections::{HashMap, HashSet};

use layer_harness::{LaneKey, Stamped};
use sip_message::{sniff, SipMessage, SipParser};

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

/// The wire-model facts of one recorded datagram.
struct Facts {
    kind: Kind,
    call_id: String,
    cseq: u32,
    cseq_method: String,
    via_branch: Option<String>,
    from_tag: Option<String>,
    to_tag: Option<String>,
    body: Vec<u8>,
}

impl Facts {
    fn of(parsed: &SipMessage) -> Facts {
        let kind = match parsed {
            SipMessage::Request(r) => Kind::Request { method: r.method().to_string() },
            SipMessage::Response(_) => Kind::Response { status: status(parsed) },
        };
        Facts {
            kind,
            call_id: call_id(parsed).to_string(),
            cseq: cseq_seq(parsed),
            cseq_method: cseq_method(parsed).to_string(),
            via_branch: top_via_branch(parsed),
            from_tag: from_tag(parsed).map(str::to_string),
            to_tag: to_tag(parsed).map(str::to_string),
            body: body_of(parsed).to_vec(),
        }
    }

    /// A datagram no reading accepts, read off its identity lines: a request
    /// refused for a line every reading refuses (a single-valued header stated
    /// twice) still names the transaction its refusal answers and the CSeq its
    /// dialog moved to (RFC 3261 §12.2.1.1). `None` where the start line, the
    /// Call-ID or the CSeq does not read: such a datagram can key nothing.
    fn sniffed(raw: &[u8]) -> Option<Facts> {
        let id = sniff::identity(raw);
        let kind = match (id.method, id.status) {
            (Some(method), None) => Kind::Request { method: method.to_string() },
            (None, Some(status)) => Kind::Response { status },
            _ => return None,
        };
        let tag = |t: String| (!t.is_empty()).then_some(t);
        Some(Facts {
            kind,
            call_id: id.call_id?.to_string(),
            cseq: id.cseq?,
            cseq_method: id.cseq_method?.to_string(),
            via_branch: id.branch.filter(|b| !b.is_empty()).map(str::to_string),
            from_tag: tag(sniff::from_tag(raw)),
            to_tag: id.to_tag.map(str::to_string),
            body: sniff::body(raw).unwrap_or_default().to_vec(),
        })
    }
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
    // The datagrams no reading accepts, per bind: a byte-identical copy of one
    // is its repeat.
    let mut unread: HashSet<(LaneKey, Vec<u8>)> = HashSet::new();
    let parser = crate::rfc_audit::lenient_parser();

    for s in events {
        let (bind, facts, repeat, src, dst, head): (
            &LaneKey,
            Facts,
            bool,
            String,
            String,
            Vec<u8>,
        ) = match &s.event {
            SignalingNetworkEvent::RecvItem { bind_key, packet, wire, .. } => {
                // Parsed once at event production.
                let (facts, repeat) = match wire.parsed.as_deref() {
                    Some(parsed) => {
                        let chain = wire.repeat_of.unwrap_or(s.seq);
                        (Facts::of(parsed), !held_chains.insert(chain))
                    }
                    None => {
                        let Some(facts) = Facts::sniffed(&packet.raw) else { continue };
                        (facts, !unread.insert((bind_key.clone(), packet.raw.to_vec())))
                    }
                };
                (
                    bind_key,
                    facts,
                    repeat,
                    packet.src.to_string(),
                    bind_key.clone(),
                    packet.raw.to_vec(),
                )
            }
            SignalingNetworkEvent::SendCalled { bind_key, to, msg } => {
                let (facts, repeat) = match parser.parse(msg) {
                    Ok(parsed) => {
                        let repeat = send_tables
                            .entry(bind_key.clone())
                            .or_default()
                            .note(msg, &parsed, s.seq)
                            .is_some();
                        (Facts::of(&parsed), repeat)
                    }
                    Err(_) => {
                        let Some(facts) = Facts::sniffed(msg) else { continue };
                        (facts, !unread.insert((bind_key.clone(), msg.clone())))
                    }
                };
                (bind_key, facts, repeat, bind_key.clone(), to.to_string(), msg.clone())
            }
            _ => continue,
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
        view.meta.push((s.seq, facts.call_id.clone()));
        view.msgs.push(Msg {
            at_us,
            src,
            dst,
            hop: 0,
            repeat,
            kind: facts.kind,
            call_id: facts.call_id,
            cseq: facts.cseq,
            cseq_method: facts.cseq_method,
            via_branch: facts.via_branch,
            from_tag: facts.from_tag,
            to_tag: facts.to_tag,
            head: Some(head),
            // The vantage IS the datagram, so the body is always carried: a
            // message with none states an EMPTY one, never an unknown one.
            body: Some(facts.body),
        });
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
