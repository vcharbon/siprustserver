//! Which recorded lanes RELAY rather than author, so a finding about what an
//! endpoint minted is judged on the lane that minted it and not on the hop that
//! passed it on.
//!
//! Per-message shape and header reads do NOT live here — see [`super::msg_reads`]
//! and `sip_message::sniff`.

use std::collections::HashSet;

use layer_harness::{LaneKey, Stamped};
use sip_message::{sniff, SipMessage, SipParser};

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::msg_reads::{call_id, from_tag, to_tag};
use crate::types::UaRole;

/// The lanes a rule that judges what a lane AUTHORED must skip: a bind that
/// declared itself `{Proxy}`-ONLY, or one that both RECEIVED and SENT one
/// dialog's establishing INVITE — same `(Call-ID, From-tag)`, no To-tag.
///
/// A B2BUA leg is not a relay: each leg carries its own Call-ID, so a leg's bind
/// only ever sends OR receives that dialog's establishing INVITE. Declaration is
/// load-bearing for the dual-face (multi-homed) proxy, whose two face binds each
/// carry ONE direction of a dialog and so can never meet the behavioural test.
pub fn relay_lanes(events: &[Stamped<SignalingNetworkEvent>]) -> HashSet<LaneKey> {
    let parser = super::lenient_parser();
    let mut lanes: HashSet<LaneKey> = HashSet::new();
    // One dialog's establishing INVITE, per direction: (bind, Call-ID, From-tag).
    let mut sent: HashSet<(LaneKey, String, String)> = HashSet::new();
    let mut received: HashSet<(LaneKey, String, String)> = HashSet::new();

    for s in events {
        let (bind_key, raw, is_sent) = match &s.event {
            SignalingNetworkEvent::BindAcquire { bind_key, summary } => {
                let roles = &summary.roles;
                if roles.len() == 1 && roles.contains(&UaRole::Proxy) {
                    lanes.insert(bind_key.clone());
                }
                continue;
            }
            SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                (bind_key, msg.as_slice(), true)
            }
            SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                (bind_key, packet.raw.as_slice(), false)
            }
            _ => continue,
        };
        // Cheap method gate before parsing — only INVITEs can establish a
        // dialog. Raw scanning is sip-message's job.
        if sniff::req_method(raw).is_none_or(|m| m != "INVITE") {
            continue;
        }
        let Ok(msg @ SipMessage::Request(_)) = parser.parse(raw) else {
            continue;
        };
        if to_tag(&msg).is_some_and(|t| !t.is_empty()) {
            continue; // a re-INVITE, not the dialog-establishing one
        }
        let cid = call_id(&msg);
        let Some(ft) = from_tag(&msg).filter(|t| !t.is_empty()) else {
            continue;
        };
        if cid.is_empty() {
            continue;
        }
        let key = (bind_key.clone(), cid.to_string(), ft.to_string());
        if is_sent {
            sent.insert(key);
        } else {
            received.insert(key);
        }
    }

    lanes.extend(sent.intersection(&received).map(|(bind, _, _)| bind.clone()));
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{BindSummary, UdpPacket};

    fn raw_invite(branch: &str, call_id: &str, ftag: &str, ttag: Option<&str>) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<sip:peer@127.0.0.1>;tag={t}"),
            None => "<sip:peer@127.0.0.1>".to_string(),
        };
        format!(
            "INVITE sip:peer@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:orig@127.0.0.1>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sent(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: "127.0.0.1:5070".parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                wire: crate::contracts::WireStamp::of_bytes(&raw),
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:5070".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    fn bind(bind: &str, roles: &[UaRole], seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::BindAcquire {
                bind_key: bind.to_string(),
                summary: BindSummary {
                    addr: "127.0.0.1:5060".parse().unwrap(),
                    queue_max: 0,
                    reuse_port: false,
                    roles: roles.iter().copied().collect(),
                    has_pre_ingress: false,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn a_hop_that_forwards_the_establishing_invite_is_a_relay() {
        let evs = vec![
            recv("lb", raw_invite("z9hG4bK-i", "c1", "at", None), 0),
            sent("lb", raw_invite("z9hG4bK-i2", "c1", "at", None), 1),
        ];
        assert!(relay_lanes(&evs).contains("lb"));
    }

    #[test]
    fn an_endpoint_that_only_originates_is_not_a_relay() {
        // The caller sent the establishing INVITE and later RECEIVED the
        // UAS-initiated re-INVITE (both tags): only a To-tag-less INVITE counts.
        let evs = vec![
            sent("alice", raw_invite("z9hG4bK-i", "c1", "at", None), 0),
            recv("alice", raw_invite("z9hG4bK-r", "c1", "bt", Some("at")), 1),
        ];
        assert!(relay_lanes(&evs).is_empty());
    }

    #[test]
    fn a_b2bua_leg_pair_is_not_a_relay() {
        // Each leg is its own Call-ID, so the two establishing INVITEs never
        // meet under one dialog key.
        let evs = vec![
            recv("b2bua", raw_invite("z9hG4bK-a", "c1", "at", None), 0),
            sent("b2bua", raw_invite("z9hG4bK-b", "c2", "bt", None), 1),
        ];
        assert!(relay_lanes(&evs).is_empty());
    }

    #[test]
    fn a_proxy_only_bind_is_a_relay_by_declaration() {
        // A dual-face proxy's face carries ONE direction, so only the
        // declaration can classify it.
        let evs = vec![
            bind("face-in", &[UaRole::Proxy], 0),
            recv("face-in", raw_invite("z9hG4bK-i", "c1", "at", None), 1),
        ];
        assert!(relay_lanes(&evs).contains("face-in"));
    }

    #[test]
    fn a_bind_declaring_a_ua_role_alongside_proxy_is_not_a_relay_by_declaration() {
        let evs = vec![
            bind("edge", &[UaRole::Proxy, UaRole::Uas], 0),
            recv("edge", raw_invite("z9hG4bK-i", "c1", "at", None), 1),
        ];
        assert!(relay_lanes(&evs).is_empty());
    }
}
