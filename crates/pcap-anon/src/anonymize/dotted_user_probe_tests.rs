//! A SIP URI user part written as a dotted quad is an address, never a number
//! with separators: the address pass fakes it as it fakes the same literal
//! anywhere else, or keeps it where the scope keeps that address, and a fake
//! address fed back in stays as it is. A dotted user part that is no quad as
//! written (an octet with a leading zero, an escape) is still a number, and
//! a separated number keeps its user parameters (RFC 4694 `;npdi;rn=`)
//! beside it. Every value is invented.

use serde_json::json;

use crate::anonymize::anonymize_in;
use crate::fake::address::{fake_ipv4, Scope};
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

/// One BYE to `sip:{user}@fake-sbc.notreal-carrier.de`, its Contact host the
/// address `contact`.
fn doc(user: &str, contact: &str) -> Flows {
    let uri = format!("sip:{user}@fake-sbc.notreal-carrier.de");
    let raw = format!(
        "BYE {uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKfakey1\r\n\
         From: \"Fakey McFakename\" <sip:+2862345678@fake-sbc.notreal-carrier.de>;tag=fakeyf\r\n\
         To: <{uri}>;tag=fakeyt\r\n\
         Call-ID: fakey-call@fake-sbc.notreal-carrier.de\r\nCSeq: 2 BYE\r\nMax-Forwards: 70\r\n\
         Contact: <sip:{contact}:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "fakey-call@fake-sbc.notreal-carrier.de",
                   "hops": [{ "a": "192.0.2.10:5060", "b": "198.51.100.20:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "192.0.2.10:5060", "dst": "198.51.100.20:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "BYE", "uri": uri,
                                           "cseq": { "seq": 2, "method": "BYE" },
                                           "from": { "uri": "sip:+2862345678@fake-sbc.notreal-carrier.de",
                                                     "tag": "fakeyf" },
                                           "to": { "uri": uri, "tag": "fakeyt" } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

fn seed() -> Seed {
    Seed::from_bytes([7; 32])
}

/// What anonymising the BYE to `user` under `scope` writes: the request-URI
/// user part, the Contact host, and the verdict total.
struct Anonymised {
    user: String,
    contact: String,
    verdict: usize,
}

fn anonymised(user: &str, contact: &str, scope: &Scope) -> Anonymised {
    let raw = doc(user, contact);
    let mut out = doc(user, contact);
    let plan = crate::testflow::plan();
    let report = anonymize_in(&mut out, &plan, &seed(), scope).expect("anonymises");
    let reading = crate::testflow::reading();
    let verdict =
        crate::verify::verdict::judge(&raw, &out, &plan, scope, &reading, &report).total();
    let wire = String::from_utf8_lossy(&crate::model::wire(&out.legs[0].msgs[0])).into_owned();
    let between = |open: &str, close: char| {
        let at = wire.find(open).expect("the probe's line") + open.len();
        wire[at..].split(close).next().expect("a closing byte").to_string()
    };
    Anonymised { user: between("BYE sip:", '@'), contact: between("Contact: <sip:", ':'), verdict }
}

#[test]
fn a_dotted_quad_user_part_is_faked_as_the_address_it_is() {
    let scope = Scope::default().all_addresses();
    let quad = "10.20.30.40";
    let out = anonymised(quad, quad, &scope);
    let fake = fake_ipv4(&seed(), quad).expect("a dotted quad");
    assert_eq!(out.user, fake, "the user part is the address's fake, no number's");
    assert_eq!(out.contact, fake, "the same address takes the same fake wherever it stands");
    assert_eq!(out.verdict, 0, "the leak check counts nothing");
}

#[test]
fn a_dotted_quad_user_part_the_scope_keeps_stays_as_written() {
    let out = anonymised("192.0.2.10", "192.0.2.10", &Scope::default());
    assert_eq!(out.user, "192.0.2.10", "a reserved address is never faked, never a number");
    assert_eq!(out.verdict, 0, "the leak check counts nothing");
}

#[test]
fn a_fake_quad_user_part_survives_a_second_pass() {
    let scope = Scope::default().all_addresses();
    let first = fake_ipv4(&seed(), "10.20.30.40").expect("a dotted quad");
    for fake in [first.as_str(), "0.31.5.180", "247.31.5.180"] {
        let out = anonymised(fake, "192.0.2.10", &scope);
        assert_eq!(out.user, fake, "a fake address is never mapped again");
        assert_eq!(out.verdict, 0, "{fake}: the leak check counts nothing");
    }
}

#[test]
fn a_dotted_user_part_that_is_no_quad_stays_a_number() {
    let scope = Scope::default().all_addresses();
    for user in ["06.23.45.67.8", "062.345.678", "286.234.567.8"] {
        let out = anonymised(user, "192.0.2.10", &scope);
        let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
        let dots = |s: &str| s.match_indices('.').map(|(i, _)| i).collect::<Vec<_>>();
        assert_ne!(
            digits(&out.user),
            digits(user),
            "{user} -> {}: its digits are mapped",
            out.user
        );
        assert_eq!(dots(&out.user), dots(user), "{user} -> {}: its separators stay", out.user);
        assert_eq!(out.verdict, 0, "{user}: the leak check counts nothing");
    }
}

#[test]
fn a_public_quad_user_part_is_faked_under_the_default_scope() {
    // Public-looking like the other probes' peers: the default scope maps
    // only public addresses.
    let quad = "8.8.8.56";
    let out = anonymised(quad, quad, &Scope::default());
    let fake = fake_ipv4(&seed(), quad).expect("a dotted quad");
    assert_eq!(out.user, fake, "the user part is the address's fake, no number's");
    assert_eq!(out.contact, fake);
    assert_eq!(out.verdict, 0, "the leak check counts nothing");
}

#[test]
fn a_separated_number_carrying_user_parameters_is_mapped() {
    let scope = Scope::default().all_addresses();
    for user in
        ["+28-68-76-54-32;npdi", "06.87.65.43.2;npdi", "+28-68-76-54-32;rn=+2861223344;npdi"]
    {
        let out = anonymised(user, "192.0.2.10", &scope);
        let head = |s: &str| s.split(';').next().unwrap_or_default().to_string();
        let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
        let (written, faked) = (head(user), head(&out.user));
        assert_ne!(
            digits(&faked),
            digits(&written),
            "{user} -> {}: its number is mapped",
            out.user
        );
        assert_eq!(faked.len(), written.len(), "{user} -> {}", out.user);
        assert!(!out.user.contains("2861223344"), "{user} -> {}: the routing number", out.user);
        assert_eq!(out.verdict, 0, "{user}: the leak check counts nothing");
    }
}

#[test]
fn an_escaped_quad_user_part_is_mapped_as_the_number_it_is_written_as() {
    for user in ["10%2E20.30.4", "10%2E20.30.40", "10.20%2E30.40", "85%2E12.34.56"] {
        let out = anonymised(user, "192.0.2.10", &Scope::default().all_addresses());
        assert_ne!(out.user, user, "the written digits are mapped");
        assert_eq!(out.verdict, 0, "{user} -> {}: the leak check counts nothing", out.user);
    }
}

/// The address class reads a user part's dotted quad, escaped or not. Under
/// the fakes of a run that wrote an escaped fake spelling, that spelling
/// alone is explained: a quad the scope maps, left in clear in a user part,
/// is counted.
#[test]
fn a_real_quad_left_in_a_user_part_is_counted_escaped_or_not() {
    let scope = Scope::default().all_addresses();
    let (plan, reading) = (crate::testflow::plan(), crate::testflow::reading());
    let mut written = doc("10%2E20.30.40", "192.0.2.10");
    let report = anonymize_in(&mut written, &plan, &seed(), &scope).expect("anonymises");
    let addresses = |flows: &Flows| {
        crate::verify::leaks::leaks(flows, &scope, &report.fakes, &reading).addresses
    };
    let wire = String::from_utf8_lossy(&crate::model::wire(&written.legs[0].msgs[0])).into_owned();
    assert!(wire.contains("%2E") && !wire.contains("10%2E20.30.40"), "an escaped fake: {wire}");
    assert_eq!(addresses(&written), 0, "the escaped spelling the run wrote is its fake");
    for user in ["10.20.30.40", "10%2E20.30.40", "10%2E20.30.41", "85%2E12.34.56"] {
        assert_eq!(addresses(&doc(user, "192.0.2.10")), 1, "{user}: the address left in clear");
    }
}
