//! The hop count (RFC 3261 §16.6 step 3) read off a received request, so a stack
//! passing one on states the count it inherited, less one.
//!
//! It is the reader for a request that CONTINUES another. A request a stack
//! sends on its own behalf — a keepalive, a teardown of its own, the ACK and
//! CANCEL a client transaction owes — states the §8.1.1.6 default and never
//! comes through here.

use crate::header::MaxForwards;
use crate::types::SipRequest;

/// The count a request the stack passes on STATES: the received count less one,
/// or — where the request states none a reader accepts —
/// [`MaxForwards::DEFAULT`] less one, this stack's convention for a sender that
/// omitted the header (RFC 3261 §16.6 step 3 asks only that a count be stated).
///
/// A spent count states 0 rather than wrapping or refilling: a request whose
/// budget is gone never regains one at a mint, so the next hop stops it even
/// where the gate that should have refused it ([`hops_exhausted`]) did not run.
///
/// Several `Max-Forwards` lines read first-wins, as [`hops_exhausted`] does, so
/// the gate and the mint can never disagree about the same message.
pub fn forwarded_max_forwards(req: &SipRequest) -> MaxForwards {
    let received = match req.header::<MaxForwards>() {
        Some(Ok(hops)) => hops,
        _ => MaxForwards::DEFAULT,
    };
    received.decremented().unwrap_or(MaxForwards::new(0))
}

/// True iff `req` arrived with a spent hop count: it may not be passed on, and
/// the receiver answers 483 Too Many Hops (or drops a request that admits no
/// response) instead of minting anything from it.
pub fn hops_exhausted(req: &SipRequest) -> bool {
    matches!(req.header::<MaxForwards>(), Some(Ok(hops)) if hops.value() == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::{SipMessage, SipParser};

    fn invite(max_forwards_line: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-a\r\n\
From: <sip:alice@example.com>;tag=alice\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: hops-test\r\n\
CSeq: 1 INVITE\r\n\
{max_forwards_line}\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::default().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(req) => req,
            _ => panic!("request"),
        }
    }

    #[test]
    fn a_stated_count_is_passed_on_one_lower() {
        assert_eq!(forwarded_max_forwards(&invite("Max-Forwards: 70\r\n")).value(), 69);
        assert_eq!(forwarded_max_forwards(&invite("Max-Forwards: 1\r\n")).value(), 0);
        assert!(!hops_exhausted(&invite("Max-Forwards: 1\r\n")));
    }

    #[test]
    fn an_absent_count_takes_the_default_one_lower() {
        assert_eq!(forwarded_max_forwards(&invite("")).value(), 69);
        assert!(!hops_exhausted(&invite("")));
    }

    #[test]
    fn a_spent_count_is_refused_and_never_refilled() {
        assert!(hops_exhausted(&invite("Max-Forwards: 0\r\n")));
        assert_eq!(forwarded_max_forwards(&invite("Max-Forwards: 0\r\n")).value(), 0);
    }
}
