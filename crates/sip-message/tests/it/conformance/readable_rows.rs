//! Accepted means readable, by a shape of each vouched header that breaks it: the
//! parser refuses the shape or its typed reader reads it. The lane's assertion
//! (`no_panic_lane::readable`) skips a header while its row here is listed.

use super::message::{read, Template};
use super::ratchet::{settle, Row};
use crate::no_panic_lane::readable::violations;

fn options() -> Template {
    Template::request("OPTIONS")
}

/// The shapes of `row` that can break the invariant.
fn shapes() -> Vec<(&'static str, Template)> {
    let digest = |value: &str| options().add(format!("Authorization: {value}"));
    let proxy_digest = |value: &str| options().add(format!("Proxy-Authorization: {value}"));
    let mut all = vec![
        ("readable/min-se", options().add("Min-SE: 90;foo=bar")),
        ("readable/min-se", options().add("Min-SE: 90;=;;")),
        ("readable/rseq", Template::response(183, "INVITE").add("RSeq: abc")),
        ("readable/rseq", Template::response(183, "INVITE").add("RSeq: 0")),
        ("readable/rseq", Template::response(183, "INVITE").add("RSeq: 1 foo")),
        ("readable/authorization", digest("Digest realm=\"a\" nonce=\"b\"")),
        ("readable/authorization", digest("Digest realm=a b")),
        ("readable/authorization", digest("Digest realm=(x)")),
        ("readable/proxy-authorization", proxy_digest("Digest realm=\"a\" nonce=\"b\"")),
        ("readable/proxy-authorization", proxy_digest("Digest realm=a b")),
        ("readable/content-type", options().add("Content-Type:")),
        (
            "readable/via-agrees",
            options()
                .with("via", "Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK-a garbage;rport"),
        ),
        (
            "readable/via",
            options().with("lower-via", "Via: SIP/2.0/UDP 192.0.2.11;branch=z9hG4bK-b;a(b=c"),
        ),
        (
            "readable/contact",
            Template::request("REGISTER")
                .with("contact", "Contact: <sip:a@192.0.2.10>,,<sip:b@192.0.2.10>"),
        ),
        ("readable/route", options().add("Route:")),
        (
            "readable/record-route",
            Template::response(180, "INVITE").add("Record-Route: <sip:192.0.2.9:65535;lr>,"),
        ),
        ("readable/session-expires", options().add("Session-Expires: 90;(x)")),
    ];
    for (row, line) in [
        ("readable/from", "From: <sip:a@example.com>;tag=a1;a(b=c"),
        ("readable/to", "To: <sip:b@example.com>;a(b=c"),
    ] {
        let slot = if row == "readable/from" { "from" } else { "to" };
        all.push((row, options().with(slot, line)));
    }
    all
}

#[test]
fn an_accepted_shape_reads() {
    let mut outcome: Vec<(&'static str, Result<(), String>)> = Vec::new();
    for (row, template) in shapes() {
        let wrong = read(&template.bytes()).ok().and_then(|msg| {
            violations(&msg).into_iter().find(|(found, _)| *found == row).map(|(_, why)| why)
        });
        match outcome.iter_mut().find(|(known, _)| *known == row) {
            Some((_, Err(_))) => {}
            Some((_, slot)) => *slot = wrong.map_or(Ok(()), Err),
            None => outcome.push((row, wrong.map_or(Ok(()), Err))),
        }
    }
    let rows: Vec<Row> = outcome.into_iter().map(|(row, result)| Row::new(row, result)).collect();
    settle("readable", &rows);
}

/// The invariant's exceptions read through it unflagged: a valid wildcard, and
/// what a final of 300 or more does not read (ADR-0053 §5).
#[test]
fn the_wildcard_and_a_final_past_299_are_not_held_to_it() {
    let star = Template::request("REGISTER").with("contact", "Contact: *").add("Expires: 0");
    let msg = read(&star.bytes()).expect("a wildcard Contact parses");
    assert!(violations(&msg).is_empty());
    let redirect = Template::response(302, "INVITE")
        .add("Contact: <>")
        .add("Route: <sip:192.0.2.9:65536;lr>")
        .add("Record-Route: <sip:192.0.2.9:65536;lr>");
    let msg = read(&redirect.bytes()).expect("a final past 299 does not read them");
    assert!(violations(&msg).is_empty());
}
