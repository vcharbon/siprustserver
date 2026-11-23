//! The datagrams the loss model dropped between a lane and a party: the
//! evidence that the lane's view of its traffic with that party is
//! incomplete, and what each dropped datagram was.

use layer_harness::{LaneKey, Stamped};
use sip_message::{sniff, SipMessage, SipParser};

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::msg_reads::{call_id, cseq_method, cseq_seq, request_method, status, to_tag};
use crate::types::RecvDisposition;

/// One datagram the loss model dropped between `lane` and the party address
/// `party`, on the call `call_id`.
struct Lost {
    lane: LaneKey,
    party: String,
    call_id: String,
    /// The party sent it to the lane, dropped on arrival (else the lane sent
    /// it to the party, dropped on send).
    toward_lane: bool,
    /// The request method; `None` for a response.
    method: Option<String>,
    /// The response status; `None` for a request.
    status: Option<u16>,
    cseq: u32,
    cseq_method: String,
    to_tag: Option<String>,
    /// The datagram as it was put on the wire.
    raw: Vec<u8>,
}

/// Every datagram the loss model dropped on a lane: one a party sent the
/// lane, dropped on arrival, or one the lane sent a party, dropped on send.
pub(crate) struct Dropped(Vec<Lost>);

impl Dropped {
    pub(crate) fn of(events: &[Stamped<SignalingNetworkEvent>]) -> Dropped {
        let mut dropped = Vec::new();
        for s in events {
            match &s.event {
                SignalingNetworkEvent::RecvItem {
                    bind_key,
                    packet,
                    wire,
                    disposition: RecvDisposition::LossModel,
                } => {
                    let Some(parsed) = wire.parsed.as_deref() else { continue };
                    let party = packet.src.to_string();
                    dropped.push(Lost::of(bind_key, party, true, parsed, &packet.raw));
                }
                SignalingNetworkEvent::SendLost { bind_key, to, msg } => {
                    let Ok(parsed) = crate::rfc_audit::lenient_parser().parse(msg) else {
                        continue;
                    };
                    dropped.push(Lost::of(bind_key, to.to_string(), false, &parsed, msg));
                }
                _ => {}
            }
        }
        Dropped(dropped)
    }

    /// Whether the loss model dropped a datagram between `lane` and `party`
    /// on the call `call_id`, in either direction.
    pub(crate) fn holds(&self, lane: &str, party: &str, call_id: &str) -> bool {
        self.0.iter().any(|l| l.between(lane, party, call_id))
    }

    /// Whether the loss model dropped, on its way from `party` to `lane` on
    /// the call `call_id`, a request that takes a CSeq number of its own (an
    /// ACK or CANCEL reuses its INVITE's) whose number `cseq` admits.
    pub(crate) fn request_to(
        &self,
        (lane, party, call_id): (&str, &str, &str),
        cseq: impl Fn(u32) -> bool,
    ) -> bool {
        self.0.iter().any(|l| {
            let numbered = l.method.as_deref().is_some_and(|m| !matches!(m, "ACK" | "CANCEL"));
            l.toward_lane && l.between(lane, party, call_id) && numbered && cseq(l.cseq)
        })
    }

    /// Whether the loss model dropped, on its way from `lane` to `party` on
    /// the call `call_id`, a copy of the request `method` `cseq`: the party
    /// took a later copy, if any.
    pub(crate) fn request_from(
        &self,
        (lane, party, call_id): (&str, &str, &str),
        method: &str,
        cseq: u32,
    ) -> bool {
        self.0.iter().any(|l| {
            !l.toward_lane
                && l.between(lane, party, call_id)
                && l.method.as_deref() == Some(method)
                && l.cseq == cseq
        })
    }

    /// Whether the loss model dropped, on its way from `party` to `lane` on
    /// the call `call_id`, a `status` final to the request `method` `cseq`
    /// carrying at least one `owed_header` row: the party issued it, and the
    /// lane took a later copy, if any.
    pub(crate) fn final_to(
        &self,
        (lane, party, call_id): (&str, &str, &str),
        (method, cseq): (&str, u32),
        (status, owed_header): (u16, &str),
    ) -> bool {
        self.0.iter().any(|l| {
            l.toward_lane
                && l.between(lane, party, call_id)
                && l.status == Some(status)
                && l.cseq == cseq
                && l.cseq_method.eq_ignore_ascii_case(method)
                && !sniff::header_values(&l.raw, owed_header).is_empty()
        })
    }

    /// Whether the loss model dropped, on its way from `party` to `lane` on
    /// the call `call_id`, a provisional above 100 carrying the To tag
    /// `to_tag`.
    pub(crate) fn provisional_to(
        &self,
        (lane, party, call_id): (&str, &str, &str),
        to_tag: &str,
    ) -> bool {
        self.0.iter().any(|l| {
            l.toward_lane
                && l.between(lane, party, call_id)
                && l.status.is_some_and(|s| (101..200).contains(&s))
                && l.to_tag.as_deref() == Some(to_tag)
        })
    }
}

impl Lost {
    fn of(lane: &LaneKey, party: String, toward_lane: bool, m: &SipMessage, raw: &[u8]) -> Lost {
        Lost {
            lane: lane.clone(),
            party,
            call_id: call_id(m).to_string(),
            toward_lane,
            method: request_method(m).map(|method| method.as_str().to_string()),
            status: Some(status(m)).filter(|s| *s != 0),
            cseq: cseq_seq(m),
            cseq_method: cseq_method(m).to_string(),
            to_tag: to_tag(m).map(str::to_string),
            raw: raw.to_vec(),
        }
    }

    fn between(&self, lane: &str, party: &str, call_id: &str) -> bool {
        self.lane == lane && self.party == party && self.call_id == call_id
    }
}
