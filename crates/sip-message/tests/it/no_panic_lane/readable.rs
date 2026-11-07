//! Accepted means readable: a message the wire parser accepted holds no line of a
//! header ingress vouches for that its typed reader refuses. A guard against a
//! second reader coming back; the conformance suite is the main check.
//!
//! A header whose row `readable/<header>` is on the conformance ratchet is left
//! out of [`assert_readable`] until the row leaves the list.

use sip_message::header::{self, HeaderValue};
use sip_message::{SipMessage, SipParseError};

/// The conformance ratchet, the one list of what is known to fail.
const RATCHET: &str = include_str!("../conformance/ratchet.txt");

fn on_the_ratchet(row: &str) -> bool {
    RATCHET.lines().any(|line| line.split(' ').next() == Some(row))
}

/// The reason `H`'s typed reader refuses `msg`'s lines of it.
fn refusal<H: HeaderValue>(msg: &SipMessage) -> Option<SipParseError> {
    msg.list::<H>().err().or_else(|| msg.header::<H>().and_then(Result::err))
}

/// A final of 300 or more establishes no dialog, so its Contact, Route and
/// Record-Route are not read (ADR-0053 §5).
fn past_299(msg: &SipMessage) -> bool {
    matches!(msg, SipMessage::Response(r) if r.status() >= 300)
}

/// The Via lines the parse read eagerly and the typed reader's reading of them:
/// as many values, each the same.
fn via_agreement(msg: &SipMessage) -> Option<String> {
    let typed = msg.list::<header::Via>().ok()?;
    let eager = msg.via().iter();
    let count = msg.via().iter().count();
    if count != typed.len() {
        return Some(format!("eager {count} values, typed {}", typed.len()));
    }
    let same = |a: &header::Via, b: &header::Via| {
        (a.host_port(), a.branch(), a.rport()) == (b.host_port(), b.branch(), b.rport())
    };
    let differ = eager.zip(typed.iter()).find(|(a, b)| !same(a, b));
    differ.map(|(a, b)| format!("eager {} typed {}", a.to_wire(), b.to_wire()))
}

/// Every row of the invariant, `(row, what failed)`, whatever the ratchet says.
pub fn violations(msg: &SipMessage) -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    let mut check = |row: &'static str, reason: Option<SipParseError>| {
        if let Some(e) = reason {
            found.push((row, e.reason));
        }
    };
    check("readable/via", refusal::<header::Via>(msg));
    check("readable/from", refusal::<header::From>(msg));
    check("readable/to", refusal::<header::To>(msg));
    if !past_299(msg) {
        if !msg.contacts().is_wildcard() {
            check("readable/contact", refusal::<header::Contact>(msg));
        }
        check("readable/route", refusal::<header::RouteEntry>(msg));
        check("readable/record-route", refusal::<header::RecordRouteEntry>(msg));
    }
    check("readable/cseq", refusal::<header::CSeq>(msg));
    check("readable/max-forwards", refusal::<header::MaxForwards>(msg));
    check("readable/content-length", refusal::<header::ContentLength>(msg));
    check("readable/expires", refusal::<header::Expires>(msg));
    check("readable/min-expires", refusal::<header::MinExpires>(msg));
    check("readable/session-expires", refusal::<header::SessionExpires>(msg));
    check("readable/min-se", refusal::<header::MinSe>(msg));
    check("readable/rseq", refusal::<header::RSeq>(msg));
    check("readable/authorization", refusal::<header::Authorization>(msg));
    check("readable/proxy-authorization", refusal::<header::ProxyAuthorization>(msg));
    check("readable/content-type", refusal::<header::MediaType>(msg));
    if let Some(differ) = via_agreement(msg) {
        found.push(("readable/via-agrees", differ));
    }
    found
}

/// Fails the lane on a violation the ratchet does not hold.
pub fn assert_readable(msg: &SipMessage) {
    for (row, reason) in violations(msg) {
        assert!(on_the_ratchet(row), "{row}: an accepted message does not read: {reason}");
    }
}
