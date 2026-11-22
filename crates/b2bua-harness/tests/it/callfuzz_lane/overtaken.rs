//! What the recording of a lossy call shows of an in-dialog request a later
//! one overtook: the network lost its first copy, its sender sent the next
//! request of the dialog after it, and its receiver refused the copy that
//! reached it 500, its CSeq gone backwards (RFC 3261 §12.2.2, §17.1.2.2).
//! The 500 is the receiver's right answer, so the call's scenario may not
//! reach the ending it was drawn for; every other net still judges it.

use layer_harness::Stamped;
use sip_message::sniff::{self, Identity};
use sip_net_testkit::SignalingNetworkEvent;

/// Whether `events` show a request whose first copy was lost refused 500
/// after its sender sent a request of the same dialog numbered above it.
pub(super) fn refused_after_being_overtaken(events: &[Stamped<SignalingNetworkEvent>]) -> bool {
    events.iter().enumerate().any(|(at, s)| match &s.event {
        SignalingNetworkEvent::SendLost { bind_key, msg, .. } => {
            let lost = sniff::identity(msg);
            let sequenced =
                lost.cseq.is_some() && lost.method.is_some_and(|m| !matches!(m, "ACK" | "CANCEL"));
            let from_tag = sniff::from_tag(msg);
            let mut overtaken = false;
            sequenced
                && events[at + 1..].iter().any(|later| match &later.event {
                    SignalingNetworkEvent::SendCalled { bind_key: lane, msg: next, .. }
                        if lane == bind_key =>
                    {
                        overtaken |= numbered_above(&lost, &from_tag, next);
                        false
                    }
                    SignalingNetworkEvent::SendCalled { msg: answer, .. } => {
                        overtaken && refuses_500(&lost, answer)
                    }
                    _ => false,
                })
        }
        _ => false,
    })
}

/// Whether `next` is a request of `lost`'s dialog (its Call-ID, From-tag and
/// To-tag) numbered above it.
fn numbered_above(lost: &Identity<'_>, from_tag: &str, next: &[u8]) -> bool {
    let id = sniff::identity(next);
    id.method.is_some_and(|m| !matches!(m, "ACK" | "CANCEL"))
        && id.call_id == lost.call_id
        && id.to_tag == lost.to_tag
        && sniff::from_tag(next) == from_tag
        && id.cseq > lost.cseq
}

/// Whether `answer` is a 500 to `lost`'s request: its Call-ID, CSeq and
/// method.
fn refuses_500(lost: &Identity<'_>, answer: &[u8]) -> bool {
    let id = sniff::identity(answer);
    id.status == Some(500)
        && id.call_id == lost.call_id
        && id.cseq == lost.cseq
        && id.cseq_method == lost.method
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use layer_harness::{Recorder, RunContext};
    use sip_net::BindUdpOpts;
    use sip_net_testkit::{
        with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork,
    };

    use super::refused_after_being_overtaken;

    const HEAD: &str = "Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-";

    /// A request of the peer `S` (192.0.2.1) on the dialog `S`/`U`.
    fn request(method: &str, cseq: u32) -> String {
        format!(
            "{method} sip:u@192.0.2.2 SIP/2.0\r\n{HEAD}{cseq}\r\nFrom: <sip:s@host>;tag=S\r\n\
             To: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\nCSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
    }

    /// The UA `U`'s `status` to that request.
    fn answer(status: u16, method: &str, cseq: u32) -> String {
        format!(
            "SIP/2.0 {status} Reason\r\n{HEAD}{cseq}\r\nFrom: <sip:s@host>;tag=S\r\n\
             To: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\nCSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// One datagram: the peer's request, or the UA's answer.
    enum Sent {
        Peer(String),
        Ua(String),
    }

    /// The fact after `sent`, on a fabric that loses the peer's first
    /// datagram when `lose`.
    async fn fact_after(lose: bool, sent: &[Sent]) -> bool {
        let peer: SocketAddr = "192.0.2.1:5060".parse().unwrap();
        let first = AtomicBool::new(lose);
        let fabric =
            SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, _| {
                if src == peer && first.swap(false, Ordering::SeqCst) {
                    Fate::Lose
                } else {
                    Fate::Deliver
                }
            }));
        let wrapped = with_all_contracts(
            Arc::new(fabric),
            Recorder::fake(),
            RunContext::TestWithRecorder,
            ScopedAuditOptions::default(),
            true,
        );
        let net = wrapped.network.clone();
        let s = net.bind_udp(BindUdpOpts::new(peer, 64)).await.unwrap();
        let u =
            net.bind_udp(BindUdpOpts::new("192.0.2.2:5060".parse().unwrap(), 64)).await.unwrap();
        for datagram in sent {
            match datagram {
                Sent::Peer(raw) => s.send_to(raw.as_bytes(), u.local_addr()).await.unwrap(),
                Sent::Ua(raw) => u.send_to(raw.as_bytes(), s.local_addr()).await.unwrap(),
            }
            net.await_in_flight(Duration::from_secs(1)).await;
        }
        refused_after_being_overtaken(&wrapped.recording.channel().snapshot())
    }

    /// The PRACK numbered 2 is lost, the UPDATE numbered 3 overtakes it, and
    /// the PRACK's retransmission is refused 500 (RFC 3261 §12.2.2).
    #[tokio::test(start_paused = true)]
    async fn a_500_to_an_overtaken_lost_request_holds() {
        let sent = [
            Sent::Peer(request("PRACK", 2)),
            Sent::Peer(request("UPDATE", 3)),
            Sent::Ua(answer(200, "UPDATE", 3)),
            Sent::Peer(request("PRACK", 2)),
            Sent::Ua(answer(500, "PRACK", 2)),
        ];
        assert!(fact_after(true, &sent).await);
    }

    /// Nothing lost, nothing overtaking the lost request, or a final other
    /// than 500 to it: the fact does not hold.
    #[tokio::test(start_paused = true)]
    async fn any_other_500_or_loss_does_not_hold() {
        let overtaken = || {
            [
                Sent::Peer(request("PRACK", 2)),
                Sent::Peer(request("UPDATE", 3)),
                Sent::Peer(request("PRACK", 2)),
                Sent::Ua(answer(500, "PRACK", 2)),
            ]
        };
        assert!(!fact_after(false, &overtaken()).await, "nothing lost");
        let alone = [
            Sent::Peer(request("PRACK", 2)),
            Sent::Peer(request("PRACK", 2)),
            Sent::Ua(answer(500, "PRACK", 2)),
        ];
        assert!(!fact_after(true, &alone).await, "not overtaken");
        let other = [
            Sent::Peer(request("PRACK", 2)),
            Sent::Peer(request("UPDATE", 3)),
            Sent::Ua(answer(500, "UPDATE", 3)),
        ];
        assert!(!fact_after(true, &other).await, "a 500 to another request");
    }
}
