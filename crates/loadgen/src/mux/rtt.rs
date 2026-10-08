//! Per-endpoint SIP round-trip timing: the exchanges whose duration holds no
//! scripted timer, measured at the mux choke points (`MuxEndpoint::send_to`
//! and the inbound delivery path) whether or not auto-retransmit is on.
//!
//! - UAC: a request's FIRST transmission (a retransmission never restarts the
//!   clock) to its first `100` and its first `18x` (INVITE, re-INVITE
//!   included), or to its final response (every non-INVITE request). An
//!   INVITE's final is not measured: it holds the callee's ring and answer.
//! - UAS: a 2xx to an INVITE, first transmission, to the ACK of that CSeq.
//!
//! A request is matched by Call-ID + CSeq number + CSeq method; an ACK, which
//! has no response, is not tracked. State is per endpoint, bounded by
//! [`MAX_PENDING`] open exchanges per direction, and dies with the endpoint.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_message::method::Method;
use sip_message::sniff::{call_id, cseq_method_token, cseq_number, req_method, resp_status};
use tokio::time::Instant;

/// One measured exchange: the bounded `exchange` label of
/// `loadgen_rtt_seconds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Exchange {
    /// INVITE first sent → first `100 Trying`.
    Invite100,
    /// INVITE first sent → first `18x`.
    Invite18x,
    /// BYE first sent → its final.
    ByeFinal,
    /// CANCEL first sent → its final.
    CancelFinal,
    /// OPTIONS first sent → its final.
    OptionsFinal,
    /// REGISTER first sent → its final.
    RegisterFinal,
    /// INFO first sent → its final.
    InfoFinal,
    /// UPDATE first sent → its final.
    UpdateFinal,
    /// PRACK first sent → its final.
    PrackFinal,
    /// SUBSCRIBE first sent → its final.
    SubscribeFinal,
    /// NOTIFY first sent → its final.
    NotifyFinal,
    /// PUBLISH first sent → its final.
    PublishFinal,
    /// MESSAGE first sent → its final.
    MessageFinal,
    /// REFER first sent → its final.
    ReferFinal,
    /// A 2xx to an INVITE first sent → the ACK of its CSeq.
    TwoXxAck,
}

impl Exchange {
    /// Every exchange, in exposition order.
    pub const ALL: [Exchange; 15] = [
        Exchange::Invite100,
        Exchange::Invite18x,
        Exchange::ByeFinal,
        Exchange::CancelFinal,
        Exchange::OptionsFinal,
        Exchange::RegisterFinal,
        Exchange::InfoFinal,
        Exchange::UpdateFinal,
        Exchange::PrackFinal,
        Exchange::SubscribeFinal,
        Exchange::NotifyFinal,
        Exchange::PublishFinal,
        Exchange::MessageFinal,
        Exchange::ReferFinal,
        Exchange::TwoXxAck,
    ];

    /// The stable `exchange` label value.
    pub const fn label(self) -> &'static str {
        match self {
            Exchange::Invite100 => "invite_100",
            Exchange::Invite18x => "invite_18x",
            Exchange::ByeFinal => "bye_final",
            Exchange::CancelFinal => "cancel_final",
            Exchange::OptionsFinal => "options_final",
            Exchange::RegisterFinal => "register_final",
            Exchange::InfoFinal => "info_final",
            Exchange::UpdateFinal => "update_final",
            Exchange::PrackFinal => "prack_final",
            Exchange::SubscribeFinal => "subscribe_final",
            Exchange::NotifyFinal => "notify_final",
            Exchange::PublishFinal => "publish_final",
            Exchange::MessageFinal => "message_final",
            Exchange::ReferFinal => "refer_final",
            Exchange::TwoXxAck => "2xx_ack",
        }
    }

    /// The request-to-final exchange of a non-INVITE method; `None` for
    /// INVITE, ACK and an extension method.
    fn final_of(method: &Method) -> Option<Exchange> {
        Some(match method {
            Method::Bye => Exchange::ByeFinal,
            Method::Cancel => Exchange::CancelFinal,
            Method::Options => Exchange::OptionsFinal,
            Method::Register => Exchange::RegisterFinal,
            Method::Info => Exchange::InfoFinal,
            Method::Update => Exchange::UpdateFinal,
            Method::Prack => Exchange::PrackFinal,
            Method::Subscribe => Exchange::SubscribeFinal,
            Method::Notify => Exchange::NotifyFinal,
            Method::Publish => Exchange::PublishFinal,
            Method::Message => Exchange::MessageFinal,
            Method::Refer => Exchange::ReferFinal,
            Method::Invite | Method::Ack | Method::Other(_) => return None,
        })
    }
}

metric_catalogue::assert_exposition_order!(
    Exchange: Invite100,
    Invite18x,
    ByeFinal,
    CancelFinal,
    OptionsFinal,
    RegisterFinal,
    InfoFinal,
    UpdateFinal,
    PrackFinal,
    SubscribeFinal,
    NotifyFinal,
    PublishFinal,
    MessageFinal,
    ReferFinal,
    TwoXxAck
);

/// Where a call's measured exchanges go (the reporter, bound to the call's
/// scenario).
pub type RttSink = Arc<dyn Fn(Exchange, Duration) + Send + Sync>;

/// The open exchanges one direction of an endpoint keeps at most; a request
/// or 2xx sent past it is not measured.
const MAX_PENDING: usize = 64;

/// A request sent and not yet finally answered.
struct Pending {
    sent: Instant,
    seen_100: bool,
    seen_18x: bool,
}

/// One endpoint's open exchanges.
#[derive(Default)]
struct RttState {
    /// Requests sent, by (Call-ID, CSeq number, CSeq method).
    requests: HashMap<(String, u32, Method), Pending>,
    /// 2xx to an INVITE sent and not yet ACKed, by (Call-ID, CSeq number).
    answers: HashMap<(String, u32), Instant>,
}

/// One endpoint's round-trip tracker: fed every datagram the endpoint sends
/// and every one delivered to it, it hands each completed exchange to its
/// sink.
pub(super) struct RttTracker {
    sink: RttSink,
    state: Mutex<RttState>,
}

impl RttTracker {
    pub(super) fn new(sink: RttSink) -> Self {
        Self { sink, state: Mutex::new(RttState::default()) }
    }

    /// A datagram this endpoint sends at `now`: a request (not an ACK) or a
    /// 2xx to an INVITE opens its exchange, unless it is already open.
    pub(super) fn on_outbound(&self, raw: &[u8], now: Instant) {
        let Some(cid) = call_id(raw) else { return };
        let Some(cseq) = cseq_number(raw) else { return };
        let mut g = self.state.lock().unwrap();
        match resp_status(raw) {
            Some(status) => {
                let invite = cseq_method_token(raw).is_some_and(|m| m == "INVITE");
                if invite && (200..300).contains(&status) && g.answers.len() < MAX_PENDING {
                    g.answers.entry((cid, cseq)).or_insert(now);
                }
            }
            None => {
                let Some(method) = req_method(raw).map(Method::from) else { return };
                if method == Method::Ack || g.requests.len() >= MAX_PENDING {
                    return;
                }
                g.requests.entry((cid, cseq, method)).or_insert(Pending {
                    sent: now,
                    seen_100: false,
                    seen_18x: false,
                });
            }
        }
    }

    /// A datagram delivered to this endpoint at `now`: a response to one of
    /// its open requests, or the ACK of one of its open 2xx.
    pub(super) fn on_inbound(&self, raw: &[u8], now: Instant) {
        let Some(cid) = call_id(raw) else { return };
        let Some(cseq) = cseq_number(raw) else { return };
        let done = {
            let mut g = self.state.lock().unwrap();
            match resp_status(raw) {
                Some(status) => {
                    let Some(method) = cseq_method_token(raw).map(Method::from) else { return };
                    response_rtt(&mut g, (cid, cseq, method), status, now)
                }
                None => match req_method(raw).map(Method::from) {
                    Some(Method::Ack) => g
                        .answers
                        .remove(&(cid, cseq))
                        .map(|sent| (Exchange::TwoXxAck, now.saturating_duration_since(sent))),
                    _ => None,
                },
            }
        };
        if let Some((exchange, rtt)) = done {
            (self.sink)(exchange, rtt);
        }
    }
}

/// The exchange a response to the open request `key` completes, if any; a
/// final closes the request.
fn response_rtt(
    g: &mut RttState,
    key: (String, u32, Method),
    status: u16,
    now: Instant,
) -> Option<(Exchange, Duration)> {
    let invite = key.2 == Method::Invite;
    if status >= 200 {
        let pending = g.requests.remove(&key)?;
        let exchange = Exchange::final_of(&key.2)?;
        return Some((exchange, now.saturating_duration_since(pending.sent)));
    }
    if !invite {
        return None;
    }
    let pending = g.requests.get_mut(&key)?;
    let exchange = match status {
        100 if !pending.seen_100 => {
            pending.seen_100 = true;
            Exchange::Invite100
        }
        180..=189 if !pending.seen_18x => {
            pending.seen_18x = true;
            Exchange::Invite18x
        }
        _ => return None,
    };
    Some((exchange, now.saturating_duration_since(pending.sent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Seen = Arc<Mutex<Vec<(Exchange, Duration)>>>;

    fn tracker() -> (RttTracker, Seen) {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        (RttTracker::new(Arc::new(move |e, d| sink.lock().unwrap().push((e, d)))), seen)
    }

    fn req(method: &str, cseq: u32, cid: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
             Call-ID: {cid}\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, method: &str, cseq: u32, cid: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} Reason\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
             Call-ID: {cid}\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A retransmitted INVITE never restarts the clock: the 100 is timed
    /// from the first transmission.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn the_first_transmission_starts_the_clock() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        t.on_outbound(&req("INVITE", 1, "a"), t0);
        t.on_outbound(&req("INVITE", 1, "a"), t0 + ms(500));
        t.on_inbound(&resp(100, "INVITE", 1, "a"), t0 + ms(600));
        assert_eq!(*seen.lock().unwrap(), vec![(Exchange::Invite100, ms(600))]);
    }

    /// Only the first 100 and the first 18x of an INVITE are timed; its
    /// final is not, and closes it.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn an_invite_times_its_first_100_and_first_18x_only() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        t.on_outbound(&req("INVITE", 1, "a"), t0);
        t.on_inbound(&resp(100, "INVITE", 1, "a"), t0 + ms(2));
        t.on_inbound(&resp(100, "INVITE", 1, "a"), t0 + ms(3));
        t.on_inbound(&resp(180, "INVITE", 1, "a"), t0 + ms(4));
        t.on_inbound(&resp(183, "INVITE", 1, "a"), t0 + ms(5));
        t.on_inbound(&resp(200, "INVITE", 1, "a"), t0 + ms(400));
        t.on_inbound(&resp(180, "INVITE", 1, "a"), t0 + ms(401));
        t.on_outbound(&req("ACK", 1, "a"), t0 + ms(402));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Exchange::Invite100, ms(2)), (Exchange::Invite18x, ms(4))]
        );
        assert!(t.state.lock().unwrap().requests.is_empty(), "the final closes the INVITE");
    }

    /// A non-INVITE request is timed to its final; its provisionals are not.
    /// A CANCEL and the INVITE it cancels share a CSeq number but are two
    /// exchanges.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_non_invite_request_times_its_final() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        t.on_outbound(&req("INVITE", 1, "a"), t0);
        t.on_outbound(&req("CANCEL", 1, "a"), t0 + ms(10));
        t.on_inbound(&resp(200, "CANCEL", 1, "a"), t0 + ms(13));
        t.on_inbound(&resp(487, "INVITE", 1, "a"), t0 + ms(14));
        t.on_outbound(&req("BYE", 2, "a"), t0 + ms(20));
        t.on_inbound(&resp(100, "BYE", 2, "a"), t0 + ms(21));
        t.on_inbound(&resp(200, "BYE", 2, "a"), t0 + ms(24));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Exchange::CancelFinal, ms(3)), (Exchange::ByeFinal, ms(4))]
        );
    }

    /// A UAS times its 2xx to an INVITE, first transmission, to the ACK of
    /// that CSeq.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_2xx_is_timed_to_its_ack() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        t.on_inbound(&req("INVITE", 7, "b"), t0);
        t.on_outbound(&resp(180, "INVITE", 7, "b"), t0 + ms(1));
        t.on_outbound(&resp(200, "INVITE", 7, "b"), t0 + ms(100));
        t.on_outbound(&resp(200, "INVITE", 7, "b"), t0 + ms(600));
        t.on_inbound(&req("ACK", 7, "b"), t0 + ms(700));
        t.on_inbound(&req("ACK", 7, "b"), t0 + ms(701));
        assert_eq!(*seen.lock().unwrap(), vec![(Exchange::TwoXxAck, ms(600))]);
    }

    /// A response or ACK of another Call-ID, CSeq number or CSeq method
    /// completes nothing.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn an_unrelated_cseq_completes_nothing() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        t.on_outbound(&req("INVITE", 1, "a"), t0);
        t.on_outbound(&req("BYE", 2, "a"), t0);
        t.on_outbound(&resp(200, "INVITE", 3, "a"), t0);
        t.on_inbound(&resp(100, "INVITE", 9, "a"), t0 + ms(1));
        t.on_inbound(&resp(180, "INVITE", 1, "other"), t0 + ms(1));
        t.on_inbound(&resp(200, "OPTIONS", 2, "a"), t0 + ms(1));
        t.on_inbound(&req("ACK", 4, "a"), t0 + ms(1));
        t.on_inbound(&req("BYE", 3, "a"), t0 + ms(1));
        assert!(seen.lock().unwrap().is_empty(), "{:?}", seen.lock().unwrap());
    }

    /// Open exchanges are bounded per endpoint: a request sent past the bound
    /// is not measured.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn open_exchanges_are_bounded() {
        let (t, seen) = tracker();
        let t0 = Instant::now();
        for n in 0..(MAX_PENDING as u32 + 8) {
            t.on_outbound(&req("OPTIONS", n, "a"), t0);
        }
        assert_eq!(t.state.lock().unwrap().requests.len(), MAX_PENDING);
        t.on_inbound(&resp(200, "OPTIONS", MAX_PENDING as u32 + 1, "a"), t0 + ms(1));
        assert!(seen.lock().unwrap().is_empty());
    }
}
