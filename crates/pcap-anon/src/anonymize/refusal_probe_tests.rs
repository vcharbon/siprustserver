//! The probes of the verify refusals met over a whole capture population,
//! every value invented: a host behind a `%40` escape in a URI parameter, a
//! number in an escaped URI of a Contact parameter, a host in a Via
//! extension parameter, a vendor header name holding a dot, a Reason
//! protocol token, a Call-ID opening on an escape, a digit run in the
//! file name of an Identity `info` URL, a dotted opaque key whose first
//! label is longer than a DNS label, a number as a display name and in an
//! escaped URI, an address as an RTCP canonical name, a Call-ID host that is
//! an opaque token, numbers of zeros only, a payload copied into a vendor
//! header, feature tags and Info Package names, a device id, and a status
//! line spelt by a Reason text. Each probe must leave none of its
//! identities, keep the datagram's length and parse, and the leak checks
//! must count nothing; untouched, they count something.

use serde_json::json;

use crate::anonymize::{anonymize_in, parse_ok_map, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

const HEAD: &str = "INVITE sip:+2861112222@callee-dom.de SIP/2.0";

/// The probe's header lines beyond the dialog's own.
fn extra(variant: char) -> Vec<&'static str> {
    match variant {
        // A host behind an escaped `@` in a URI parameter of the Contact.
        'a' => vec![
            "Contact: <sip:mx77;ab=1;user-id=ABCD%40ringo.telcoz-a.de@85.12.34.20:5060;transport=udp>",
        ],
        // A number and a host in an escaped URI of a Contact parameter.
        'b' => vec![
            "Contact: <sip:4321@85.12.34.20:5060;asm=1;epv=%3Csip:5551234%40ims.telcoz-b.de%3Bgr%3Durn:uuid:0f6c2a1e-1b2c-4d5e-8f90-a1b2c3d4e5f6%3E>",
        ],
        // A host in a Via extension parameter.
        'c' => vec![
            "Contact: <sip:4321@85.12.34.20:5060>",
            "Via: SIP/2.0/UDP 85.12.34.21:5060;branch=z9hG4bKc9;pterm=edge7.trunkz-c.de;initialInvite=true",
        ],
        // A vendor header whose name holds a dot.
        'd' => vec!["Contact: <sip:4321@85.12.34.20:5060>", "P-Vendorx.Session-Info: corrID=ab1"],
        // A Reason whose protocol is a dotted token.
        'e' => vec!["Contact: <sip:4321@85.12.34.20:5060>", "Reason: X.vnd;reasoncode=0x00001234"],
        // A digit run in the file name of an Identity `info` URL.
        'f' => vec![
            "Contact: <sip:4321@85.12.34.20:5060>",
            "Identity: eyJhbGciOiJFUzI1NiJ9.eyJvcmlnIjp7InRuIjoiMjg2MTExMzMzMyJ9fQ.c2ln;info=<https://cert.sti-f.de/certs/AB12/1234a5bc73519024.pem>;alg=ES256;ppt=shaken",
        ],
        // A number written as a display name and in an escaped URI of a Contact parameter.
        'i' => vec![
            "P-Asserted-Identity: \"5551239\" <sip:4321@85.12.34.20>",
            "Contact: <sip:4321@85.12.34.20:5060;asm=1;epv=%3Csip:5551239%40ims.telcoz-i.de%3Bgr%3D12345a678%3E>",
        ],
        // A number carrying a user parameter, written only there.
        'k' => vec![
            "P-Asserted-Identity: <sip:+2865454545;verstat=TN-Validation-Passed@caller-dom.de;user=phone>",
        ],
        // A short private number standing as a dotted group of a vendor parameter.
        'm' => vec![
            "P-Asserted-Identity: <sip:4321@caller-dom.de>",
            "Contact: <sip:85.12.34.20:5060;yop=12.34.56789ABC.4321.0042>",
        ],
        // A long digit run standing alone in a vendor header and as the tail
        // of a longer number elsewhere.
        'n' => vec![
            "P-Asserted-Identity: <sip:61234598765432101234567890@caller-dom.de>",
            "P-Vendor-Pani: wlan;line-id=98765432101234567890;cc=28",
        ],
        // A conversation id written as a User-to-User payload and in a vendor header.
        'q' => vec![
            "X-Vendor-Cnv: 1f234567-89ab-4c2d-9e12-3a4b56789012",
            "User-to-User: 1f234567-89ab-4c2d-9e12-3a4b56789012",
        ],
        // A short private number written as, or at the end of, a display name,
        // and spelt by a group of an instance id.
        't' => vec![
            "Remote-Party-ID: \"Fakey McFakename 52417\" <sip:52417@85.12.34.5>;party=calling;screen=yes;privacy=off",
            "P-Asserted-Identity: \"52417\" <sip:52417@85.12.34.5:5060;user=phone>",
            "Diversion: <sip:524190@85.12.34.5>;reason=unconditional",
            "Contact: <sip:4321@85.12.34.20:5060>;+sip.instance=\"<urn:gsma:imei:35000099-524190-1>\"",
        ],
        // A feature tag of the global tree written without its `+`.
        'r' => vec!["Contact: <sip:4321@85.12.34.20:5060>;g.3gpp.zz-vndr-flag;expires=60"],
        // Info Package names (RFC 6086) of the global tree.
        's' => vec!["Recv-Info: g.3gpp.zz-vndr-pkg, g.3gpp.yy-other-pkg"],
        // Numbers of zeros only, and a run of zeros in a vendor header.
        'p' => vec![
            "P-Asserted-Identity: <sip:+2800000000@caller-dom.de>",
            "Referred-By: <sip:0000@caller-dom.de>",
            "P-Vendor-Pani: wlan;line-id=00000000000000000000;cc=28",
        ],
        // A long digit run spelling a national number of the document inside it.
        'o' => vec![
            "P-Asserted-Identity: <sip:+2865454545@caller-dom.de>",
            "P-Vendor-Pani: wlan;line-id=1234565454545678901;cc=28",
        ],
        // A dotted opaque key with a label longer than a DNS label, as a URI parameter.
        'h' => vec![
            "Contact: <sip:85.12.34.20:5060;line=kq-zv7fwqe2plmn8rstuvw3xyzab4cdefg5hijkl6mnopq7rstuv8wxyzab9cdefghijklmnopqr.7quokka9lemur.zebra2;transport=udp>",
        ],
        _ => vec![],
    }
}

/// The probe's Call-ID.
fn call_id(variant: char) -> &'static str {
    match variant {
        // A Call-ID opening on an escape, a digit run right behind it.
        'g' => "T%Ab4455667788QwErTyUi@cidhost.callerdom.de",
        // A Call-ID whose host part is an opaque token, digit runs inside.
        'l' => "ab12cd34567ef8g901h2345@1-ABc2de345-678f7654321gh890i1jkl2mno3456p7q-r890s123",
        _ => "c1@cidhost.callerdom.de",
    }
}

/// The probe's SDP body, empty for none.
fn body(variant: char) -> &'static str {
    match variant {
        // An address as the RTCP canonical name of a source (RFC 5576 `cname`).
        'j' => {
            "v=0\r\no=- 1 1 IN IP4 85.12.34.30\r\ns=-\r\nc=IN IP4 85.12.34.30\r\nt=0 0\r\n\
                m=audio 4000 RTP/AVP 0\r\na=ssrc:4242 cname:85.12.34.77\r\n"
        }
        _ => "",
    }
}

/// The identities each probe carries, none of which may survive.
fn identities(variant: char) -> &'static [&'static str] {
    match variant {
        'a' => &["ringo", "telcoz-a"],
        'b' => &["5551234", "telcoz-b"],
        'c' => &["edge7", "trunkz-c"],
        'f' => &["sti-f", "73519024"],
        'g' => &["4455667788", "callerdom"],
        'h' => &["zv7fwqe2plmn8", "quokka", "lemur", "zebra2"],
        'i' => &["5551239", "telcoz-i"],
        'j' => &["85.12.34.30", "85.12.34.77"],
        'k' => &["65454545"],
        'l' => &["7654321", "34567"],
        'm' => &["4321"],
        'n' => &["98765432101234567890", "4321012345678"],
        'o' => &["65454545", "1234565454545678901"],
        't' => &["52417", "524190", "McFakename"],
        _ => &[],
    }
}

fn probe(variant: char) -> Flows {
    document(&extra(variant), call_id(variant), body(variant))
}

/// A one-INVITE document: the dialog's own header lines, `extra`, a Contact
/// where `extra` holds none, and an SDP `body` where one is given.
pub(super) fn document(extra: &[&str], call_id: &str, body: &str) -> Flows {
    let call_id_line = format!("Call-ID: {call_id}");
    let mut lines = vec![
        HEAD,
        "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKp1",
        "From: <sip:+2861113333@caller-dom.de>;tag=p2",
        "To: <sip:+2861112222@callee-dom.de>",
        &call_id_line,
        "CSeq: 2 INVITE",
        "Max-Forwards: 70",
    ];
    lines.extend(extra);
    if !lines.iter().any(|l| l.starts_with("Contact:")) {
        lines.push("Contact: <sip:4321@85.12.34.20:5060>");
    }
    let length = format!("Content-Length: {}", body.len());
    if !body.is_empty() {
        lines.push("Content-Type: application/sdp");
    }
    lines.push(&length);
    let raw = format!("{}\r\n\r\n{body}", lines.join("\r\n"));
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": call_id, "hops": [{ "a": "85.12.34.10:5060", "b": "172.16.245.246:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "85.12.34.10:5060", "dst": "172.16.245.246:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "INVITE",
                                           "uri": "sip:+2861112222@callee-dom.de",
                                           "cseq": { "seq": 2, "method": "INVITE" },
                                           "from": { "uri": "sip:+2861113333@caller-dom.de", "tag": "p2" },
                                           "to": { "uri": "sip:+2861112222@callee-dom.de", "tag": null } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

pub(super) fn wire(flows: &Flows) -> String {
    String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned()
}

/// Every leak count of `--verify` over one anonymisation, summed.
pub(super) fn leaks(raw: &Flows, out: &Flows, fakes: &Fakes) -> usize {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    crate::verify::survivors(raw, out, &plan, &scope)
        + crate::verify::unmasked_displays(out, &scope)
        + crate::verify::names::name_survivors(raw, out, &scope)
        + crate::verify::leaks::leaks(out, &scope, fakes, &crate::testflow::reading()).total()
}

const VARIANTS: [char; 20] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't',
];

#[test]
fn every_identity_of_every_refusal_probe_is_gone_and_verify_counts_nothing() {
    for variant in VARIANTS {
        let raw = probe(variant);
        let mut out = probe(variant);
        let parsed = parse_ok_map(&out, &crate::testflow::reading());
        let report = anonymize_in(
            &mut out,
            &crate::testflow::plan(),
            &Seed::from_bytes([7; 32]),
            &Scope::default(),
        )
        .expect("anonymises");
        let text = wire(&out).to_lowercase();
        for id in identities(variant) {
            assert!(
                !text.contains(&id.to_lowercase()),
                "probe {variant}: {id} survives:\n{}",
                wire(&out)
            );
        }
        assert_eq!(
            crate::model::wire(&raw.legs[0].msgs[0]).len(),
            crate::model::wire(&out.legs[0].msgs[0]).len(),
            "probe {variant}: length"
        );
        assert_eq!(
            parse_ok_map(&out, &crate::testflow::reading()),
            parsed,
            "probe {variant}: still parses"
        );
        let unexplained = crate::verify::leaks::unexplained(
            &out,
            &Scope::default(),
            &report.fakes,
            &crate::testflow::reading(),
        );
        assert_eq!(
            leaks(&raw, &out, &report.fakes),
            0,
            "probe {variant}: verify counts a leak {unexplained:?}:\n{}",
            wire(&out)
        );
    }
}

/// A 487 whose Reason text spells its own status line: both are protocol
/// text, and the status line and the Reason stay exactly as written.
#[test]
fn a_reason_text_spelling_the_status_line_leaves_both_as_written() {
    let raw = "SIP/2.0 487 Request Terminated\r\n\
               Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKp1\r\n\
               From: <sip:+2861113333@caller-dom.de>;tag=p2\r\n\
               To: <sip:+2861112222@callee-dom.de>;tag=p3\r\n\
               Call-ID: c1@cidhost.callerdom.de\r\nCSeq: 2 INVITE\r\n\
               Reason: SIP;cause=487;text=\"487 Request Terminated\"\r\n\
               Content-Length: 0\r\n\r\n";
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "c1@cidhost.callerdom.de", "hops": [{ "a": "85.12.34.10:5060", "b": "172.16.245.246:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "172.16.245.246:5060", "dst": "85.12.34.10:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "response", "status": 487, "reason": "Request Terminated",
                                           "cseq": { "seq": 2, "method": "INVITE" },
                                           "from": { "uri": "sip:+2861113333@caller-dom.de", "tag": "p2" },
                                           "to": { "uri": "sip:+2861112222@callee-dom.de", "tag": "p3" } } }] }],
        "groups": []
    });
    let raw = ingest_str(&doc.to_string()).expect("the probe ingests");
    let mut out = ingest_str(&doc.to_string()).expect("the probe ingests");
    let report = anonymize_in(
        &mut out,
        &crate::testflow::plan(),
        &Seed::from_bytes([7; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    let wire = wire(&out);
    assert!(
        wire.starts_with("SIP/2.0 487 Request Terminated\r\n"),
        "the status line moved:\n{wire}"
    );
    assert!(wire.contains("text=\"487 Request Terminated\""), "the Reason text moved:\n{wire}");
    assert_eq!(leaks(&raw, &out, &report.fakes), 0, "verify counts a leak:\n{wire}");
}

/// A display name's number takes its fake once: where that fake is itself a
/// number of the raw document, the digit pass does not map it again (in a
/// small fake space, twice can lead back to the raw number).
#[test]
fn a_masked_display_is_never_mapped_again() {
    let seed = Seed::from_bytes([7; 32]);
    let plan = crate::testflow::plan();
    let fake = crate::fake::fake_for(&plan, &seed, "52417");
    let raw_text = wire(&probe('d'))
        .replace(
            "Contact: <sip:4321@",
            &format!("Diversion: <sip:{fake}@caller-dom.de>\r\nContact: <sip:4321@"),
        )
        .replace(
            "From: <sip:",
            "From: \"52417\" <sip:52417@caller-dom.de>;tag=p9\r\nP-Asserted-Identity: <sip:",
        );
    let mut raw = probe('d');
    crate::model::set_wire(&mut raw.legs[0].msgs[0], raw_text.as_bytes());
    let mut out = raw.clone();
    let report = anonymize_in(&mut out, &plan, &seed, &Scope::default()).expect("anonymises");
    let text = wire(&out);
    assert!(
        text.contains(&format!("From: \"{fake}\" <sip:")),
        "the display shows its number's fake:\n{text}"
    );
    assert_eq!(leaks(&raw, &out, &report.fakes), 0, "verify counts a leak:\n{text}");
}

/// A fake that spells another name of the raw document where the raw bytes
/// held something else is a coincidence; the same name left in place is a
/// survivor.
#[test]
fn a_name_counts_as_a_survivor_only_where_it_was_left_in_place() {
    let raw = probe('d');
    let with = |text: String| {
        let mut out = probe('d');
        crate::model::set_wire(&mut out.legs[0].msgs[0], text.as_bytes());
        crate::verify::names::name_survivors(&raw, &out, &Scope::default())
    };
    let base = wire(&raw).replace("caller-dom.de", "zzzzzz-zzz.zz");
    let coincidence = base.replacen("callee-dom.de>", "caller-dom.de>", 1);
    assert_eq!(with(coincidence), with(base.clone()), "the To host spells another name");
    let left = base.replacen("zzzzzz-zzz.zz", "caller-dom.de", 1);
    assert_eq!(with(left), with(base) + 1, "the From host left in place");
}

#[test]
fn verify_counts_what_a_leaking_probe_leaves_when_nothing_anonymises_it() {
    for variant in ['a', 'b', 'c', 'f', 'g', 'h', 'i', 'j', 'k', 'l'] {
        let raw = probe(variant);
        assert!(leaks(&raw, &probe(variant), &Fakes::default()) > 0, "probe {variant}");
    }
}
