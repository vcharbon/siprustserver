//! The fourth privacy review's probes, every value invented: dialogs that
//! differ only in a short digit run (the pigeonhole of a small fake space),
//! `label:digits` that is no port, an `Event` dialog package, a free-text SDP
//! label, encoded payloads, vendor data. The anonymiser must keep every
//! dialog apart, leave none of the identities, and verify must count nothing;
//! untouched or merged, verify counts. The peer address is public-looking
//! (85.12.34.x) on purpose: the scope maps only public addresses.

use std::collections::HashSet;

use serde_json::json;

use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::identities::body_types::{BodyReading, BodyType};
use crate::identities::BodyTypes;
use crate::model::{ingest_str, Flows};

/// One leg per datagram of `raws`.
fn doc(raws: &[String]) -> Flows {
    let legs: Vec<serde_json::Value> = raws
        .iter()
        .map(|raw| {
            let call_id = raw.lines().find_map(|l| l.strip_prefix("Call-ID: ")).expect("a Call-ID");
            json!({ "call_id": call_id, "hops": [{ "a": "8.8.8.10:5060", "b": "172.16.245.246:5060" }],
                    "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                    "msgs": [{ "ts_us": 0, "src": "8.8.8.10:5060", "dst": "172.16.245.246:5060", "hop": 0,
                               "retx": false, "raw": raw,
                               "summary": { "kind": "request", "method": "INVITE",
                                            "uri": "sip:+2861112222@callee-dom.de",
                                            "cseq": { "seq": 2, "method": "INVITE" },
                                            "from": { "uri": "sip:+2861113333@caller-dom.de", "tag": null },
                                            "to": { "uri": "sip:+2861112222@callee-dom.de", "tag": null } } }] })
        })
        .collect();
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": raws.len(), "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": legs, "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

/// An INVITE with `extra` header lines and an optional typed body.
fn invite(
    call_id: &str,
    ftag: &str,
    ttag: &str,
    branch: &str,
    extra: &str,
    body: Option<(&str, &str)>,
) -> String {
    let (typed, content) = match body {
        Some((media, content)) => (format!("Content-Type: {media}\r\n"), content),
        None => (String::new(), ""),
    };
    format!(
        "INVITE sip:+2861112222@callee-dom.de SIP/2.0\r\n\
         Via: SIP/2.0/UDP 8.8.8.10:5060;branch=z9hG4bK{branch}\r\n\
         From: <sip:+2861113333@caller-dom.de>;tag={ftag}\r\nTo: <sip:+2861112222@callee-dom.de>;tag={ttag}\r\n\
         Call-ID: {call_id}@cidhost.callerdom.zz\r\nCSeq: 2 INVITE\r\nMax-Forwards: 70\r\n\
         Contact: <sip:+2861113333@8.8.8.10:5060>\r\n{extra}{typed}Content-Length: {}\r\n\r\n{content}",
        content.len()
    )
}

fn wire(flows: &Flows) -> String {
    flows
        .legs
        .iter()
        .flat_map(|l| &l.msgs)
        .map(|m| String::from_utf8_lossy(&crate::model::wire(m)).into_owned())
        .collect()
}

/// Every check of `--verify`, summed.
fn verdict(raw: &Flows, out: &Flows, report: &crate::anonymize::Report) -> usize {
    crate::verify::verdict::judge(
        raw,
        out,
        &crate::testflow::plan(),
        &Scope::default(),
        &crate::testflow::reading(),
        report,
    )
    .total()
}

fn anonymised(raws: &[String]) -> (Flows, Flows, crate::anonymize::Report) {
    let raw = doc(raws);
    let mut out = doc(raws);
    let report = anonymize_in(
        &mut out,
        &crate::testflow::plan(),
        &Seed::from_bytes([9; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    (raw, out, report)
}

#[test]
fn eleven_dialogs_differing_in_a_five_digit_run_stay_eleven() {
    let raws: Vec<String> = (0..11)
        .map(|n| {
            let run = format!("4{n:04}");
            // The run is also a caller's extension: a key of the number pass,
            // whose five-digit fakes merge.
            let extra = format!("P-Asserted-Identity: <sip:{run}@caller-dom.de>\r\n");
            invite(
                &format!("{run}-77ab"),
                &format!("t{run}"),
                &format!("u{run}"),
                &format!("{run}x"),
                &extra,
                None,
            )
        })
        .collect();
    let (raw, out, report) = anonymised(&raws);
    let ids = |kind: &str| -> HashSet<String> {
        out.legs
            .iter()
            .flat_map(|l| &l.msgs)
            .filter_map(|m| {
                let text = String::from_utf8_lossy(&crate::model::wire(m)).into_owned();
                text.lines().find(|l| l.starts_with(kind)).map(str::to_string)
            })
            .collect()
    };
    for kind in ["Call-ID:", "From:", "To:", "Via:"] {
        assert_eq!(ids(kind).len(), 11, "{kind}: {}", wire(&out));
    }
    assert!(!wire(&out).contains("40003"), "{}", wire(&out));
    assert!(report.shared_keys() > 0, "the extensions share fakes");
    assert_eq!(report.merged_call_ids, 0);
    assert_eq!(verdict(&raw, &out, &report), 0, "{}", wire(&out));
}

#[test]
fn merged_dialogs_are_counted() {
    let raws: Vec<String> =
        (0..3).map(|n| invite(&format!("c{n}"), "f", "t", &format!("b{n}"), "", None)).collect();
    let (raw, mut out, report) = anonymised(&raws);
    let first = crate::model::wire(&out.legs[0].msgs[0]);
    crate::model::set_wire(&mut out.legs[1].msgs[0], &first);
    assert!(crate::verify::dialogs::merged(&raw, &out, &crate::testflow::reading()) > 0);
    assert!(verdict(&raw, &out, &report) > 0);
}

#[test]
fn a_label_and_its_digits_is_no_port_and_every_identity_beside_the_dialog_goes() {
    let extra = "X-Caller: cli:061113339\r\nX-Sub: msisdn:2861119999\r\n\
                 Event: dialog;call-id=c1@cidhost.callerdom.zz;from-tag=f1abcd;to-tag=t1abcd\r\n\
                 User-to-User: 00ff01fe02;encoding=hex\r\n";
    let body = "<mscivr><caller ani=\"ani:061117777\"/></mscivr>";
    let raws = vec![
        invite("c1", "f1abcd", "t1abcd", "b1", extra, Some(("application/mediaservercontrol+xml", body))),
        invite("c2", "f2abcd", "t2abcd", "b2", "", Some(("application/sdp", "v=0\r\no=- 1 1 IN IP4 8.8.8.10\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=label:Fakey McFakename\r\n"))),
    ];
    let (raw, out, report) = anonymised(&raws);
    let text = wire(&out);
    for id in ["61113339", "61119999", "61117777", "f1abcd", "t1abcd", "McFakename", "00ff01fe02"] {
        assert!(!text.contains(id), "{id} survives:\n{text}");
    }
    let event = text.lines().find(|l| l.starts_with("Event:")).expect("the Event line");
    let from = text.lines().find(|l| l.starts_with("From:")).expect("a From line");
    let ftag = from.rsplit("tag=").next().unwrap();
    assert!(
        event.contains(&format!("from-tag={ftag}")),
        "the dialog package names the rewritten tag: {event} / {from}"
    );
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
    let untouched = crate::verify::leaks::leaks(
        &raw,
        &Scope::default(),
        &Fakes::default(),
        &crate::testflow::reading(),
    );
    assert!(untouched.digits >= 3, "{untouched:?}");
}

#[test]
fn an_extension_map_and_a_feature_tag_tree_are_vocabulary() {
    let sdp = "v=0\r\no=- 1 1 IN IP4 8.8.8.10\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n\
               a=extmap:1/sendonly http://ext.vendor-uri.zz/abs-send-time\r\n";
    let raw = invite("c1", "f1", "t1", "b1", "", Some(("application/sdp", sdp))).replace(
        "Contact: <sip:+2861113333@8.8.8.10:5060>",
        "Contact: <sip:+2861113333@8.8.8.10:5060>;+g.poc!vendor.example-tree.zz=\"x\"",
    );
    let untouched = crate::verify::leaks::leaks(
        &doc(std::slice::from_ref(&raw)),
        &Scope::default(),
        &Fakes::default(),
        &crate::testflow::reading(),
    );
    assert_eq!(untouched.domains, 3, "the three hosts, neither vocabulary token: {untouched:?}");
    let (raw, out, report) = anonymised(&[raw]);
    assert_eq!(verdict(&raw, &out, &report), 0, "{}", wire(&out));
}

#[test]
fn names_behind_a_number_in_a_uri_parameter_go_and_a_reason_text_stays() {
    let extra = "P-Asserted-Identity: <sip:+2861113333.tagx.ac14cc94@caller-dom.de>\r\n\
                 Diversion: <sip:+2861114444;tg=TRUNKGRP123.SITEZ456@caller-dom.de>\r\n\
                 Reason: Q.850;cause=16;Location=U;Location-Text=\"Desk room (FR) fake-sbc.notreal-carrier.de\"\r\n\
                 X-Attach: KEY_a:{\\\"key\\\":\\\"\\\"}|KEY_b:app_main_flow2.vxml\r\n";
    let raws = vec![invite("c1", "f1", "t1", "b1", extra, None)];
    let (raw, out, report) = anonymised(&raws);
    let text = wire(&out);
    for id in ["ac14cc94", "tagx", "TRUNKGRP123", "SITEZ456", "app_main_flow2"] {
        assert!(!text.contains(id), "{id} survives:\n{text}");
    }
    // A Reason's text is protocol text, kept byte for byte.
    assert!(
        text.contains("Location-Text=\"Desk room (FR) fake-sbc.notreal-carrier.de\""),
        "{text}"
    );
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
}

#[test]
fn a_derived_call_id_keeps_its_base_as_its_tail() {
    // The platform derives a b-leg Call-ID by prefixing the base: the cut pairs
    // the legs on that, so the anonymised pair must keep it.
    let base = "FAKEab0123-0123456789abcdef0123456789abcdef-v300g00010";
    let derived = format!("9-{base}");
    // A re-originating server names the call it derives from behind its own
    // Call-ID's `@`: an undotted word there is that identifier, no host.
    let reoriginated = format!("FAKEcd4567-0123456789abcdef0123456789abcdef-v300g00011@{derived}");
    // The base also stands whole in a vendor header's quoted string: a copy
    // of the identifier, which the identifier pass maps like the Call-ID.
    let copy = format!("X-Fake-Attach: \"{base}\"\r\n");
    let raws = vec![
        invite(base, "f1", "t1", "b1", &copy, None)
            .replace(&format!("{base}@cidhost.callerdom.zz"), base),
        invite(&derived, "f2", "t2", "b2", "", None)
            .replace(&format!("{derived}@cidhost.callerdom.zz"), &derived),
        invite(&reoriginated, "f3", "t3", "b3", "", None)
            .replace(&format!("{reoriginated}@cidhost.callerdom.zz"), &reoriginated),
    ];
    let (raw, out, report) = anonymised(&raws);
    let (a, b, c) = (&out.legs[0].call_id, &out.legs[1].call_id, &out.legs[2].call_id);
    assert_ne!(a, base, "{}", wire(&out));
    assert!(b.ends_with(a.as_str()) && b.len() == a.len() + 2, "{a} / {b}");
    assert!(c.ends_with(&format!("@{b}")) && c.len() == reoriginated.len(), "{b} / {c}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{}", wire(&out));
}

#[test]
fn a_host_spelling_a_call_id_word_never_maps_it_apart_from_its_derived_copy() {
    // `sbcfake01.notreal-carrier.de` is a host (a Via's) and spelt by the
    // words of the Call-IDs: the base bounds it, the derived copy glues it
    // behind `9-`. The identifier pass maps it in both, so the derivation holds.
    let base = "sbcfake01.notreal-carrier.de.77ab";
    let derived = format!("9-{base}");
    let via = "Via: SIP/2.0/UDP sbcfake01.notreal-carrier.de:5060;branch=z9hG4bKv1\r\n";
    let raws = vec![
        invite(base, "f1", "t1", "b1", via, None)
            .replace(&format!("{base}@cidhost.callerdom.zz"), base),
        invite(&derived, "f2", "t2", "b2", "", None)
            .replace(&format!("{derived}@cidhost.callerdom.zz"), &derived),
    ];
    let (raw, out, report) = anonymised(&raws);
    let (a, b) = (&out.legs[0].call_id, &out.legs[1].call_id);
    assert!(!wire(&out).contains("sbcfake01"), "{}", wire(&out));
    assert!(b.ends_with(a.as_str()) && b.len() == a.len() + 2, "{a} / {b}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{}", wire(&out));
}

#[test]
fn a_product_name_a_branch_also_spells_goes_wherever_it_stands_in_any_case() {
    // The SDP origin's username names a product; a branch spells it too, and
    // a Reason's protocol slot writes it in another case.
    let sdp =
        "v=0\r\no=Fakeswitch 1 1 IN IP4 8.8.8.10\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let raw = invite(
        "c1",
        "f1",
        "t1",
        "Fakeswitch.2abc",
        "Reason: FAKESWITCH;cause=1\r\n",
        Some(("application/sdp", sdp)),
    );
    let (raw, out, report) = anonymised(&[raw]);
    let text = wire(&out);
    assert!(!text.to_ascii_lowercase().contains("fakeswitch"), "{text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
}

#[test]
fn a_digit_only_name_never_reaches_an_sdp_origin_s_session_numbers() {
    // An inter-operator id of digits is a name; the SDP origin spells it as
    // its session id and version, protocol numbers that keep their step.
    let sdp = "v=0\r\no=- 4417 4417 IN IP4 8.8.8.10\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let extra = "P-Charging-Vector: icid-value=fake1234ab;orig-ioi=4417\r\n";
    let raw = invite("c1", "f1", "t1", "b1", extra, Some(("application/sdp", sdp)));
    let (raw, out, report) = anonymised(&[raw]);
    let text = wire(&out);
    assert!(text.contains("o=- 4417 4417 IN IP4"), "{text}");
    assert!(!text.contains("orig-ioi=4417"), "{text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
}

#[test]
fn an_sdp_origin_s_session_id_maps_a_phone_number_and_its_version_is_a_counter() {
    // The caller's extension and national number are number keys. A session
    // id spelling the national number (in any form) is the number and maps,
    // the same in every offer; one spelling the extension is a counter and
    // stays; the username is identity and maps.
    let extra = "P-Asserted-Identity: <sip:4417@caller-dom.de>\r\nX-Called: 2862345678\r\n";
    let origin = |o: &str| {
        let sdp =
            format!("v=0\r\n{o} IN IP4 8.8.8.10\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n");
        invite("c1", "f1", "t1", "b1", extra, Some(("application/sdp", &sdp)))
    };
    let (raw, out, report) = anonymised(&[origin("o=4417 2862345678 5")]);
    let text = wire(&out);
    assert!(!text.contains("2862345678") && !text.contains("o=4417 "), "{text}");
    assert!(text.contains(" 5 IN IP4"), "{text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
    let (raw, out, report) = anonymised(&[origin("o=- 062345678 4417")]);
    let text = wire(&out);
    assert!(!text.contains("062345678") && text.contains(" 4417 IN IP4"), "{text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
    // A version spelling the number stays a counter, and verify counts it.
    let (raw, out, report) = anonymised(&[origin("o=- 5 2862345678")]);
    let text = wire(&out);
    assert!(text.contains(" 5 2862345678 IN IP4"), "{text}");
    assert!(verdict(&raw, &out, &report) > 0, "{text}");
}

#[test]
fn an_undotted_call_id_host_of_short_labels_is_faked_whole() {
    let raws = vec![
        invite("c1", "f1", "t1", "b1", "", None)
            .replace("c1@cidhost.callerdom.zz", "fakecid01@pbx"),
        invite("c2", "f2", "t2", "b2", "", None)
            .replace("c2@cidhost.callerdom.zz", "fakecid02@gw1-7"),
    ];
    let (raw, out, report) = anonymised(&raws);
    let ids: Vec<&str> = out.legs.iter().map(|l| l.call_id.as_str()).collect();
    assert!(!ids[0].ends_with("@pbx") && !ids[1].ends_with("@gw1-7"), "{ids:?}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{}", wire(&out));
}

#[test]
fn an_ipv6_literal_inside_a_call_id_maps_whole() {
    // Global unicast on purpose: the scope keeps the documentation range.
    let raws = vec![
        invite("c1", "f1", "t1", "b1", "", None)
            .replace("c1@cidhost.callerdom.zz", "fakecid03@[3fff:e0a:1:2::77]"),
        invite("c2", "f2", "t2", "b2", "", None).replace(
            "c2@cidhost.callerdom.zz",
            "fakecid04-3fff:e0a:1:2::78@fake.notreal-carrier.de",
        ),
    ];
    let (raw, out, report) = anonymised(&raws);
    let text = wire(&out);
    assert!(!text.contains("3fff") && !text.contains("e0a:1:2"), "{text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
}

/// Two orders of a vendor type, one binary and one text: the type a scope
/// states as digit runs has its binary order redacted and its text one read,
/// its number mapped; a scope that does not state it redacts both.
#[test]
fn a_binary_digit_run_order_is_redacted_and_a_text_one_is_read() {
    let binary = "a\u{1}b\u{2}\u{3}";
    let text = "TRANSFER 061116666";
    let order = "application/vnd.example.order";
    let raws = vec![
        invite("c1", "f1", "t1", "b1", "", Some((order, binary))),
        invite("c2", "f2", "t2", "b2", "", Some((order, text))),
    ];
    let (_, unstated, report) = anonymised(&raws);
    assert_eq!(report.redacted_parts, 2, "{}", wire(&unstated));
    let types = BodyTypes::new(vec![BodyType::of(order, BodyReading::DigitRuns)])
        .expect("a bare media type");
    let mut out = doc(&raws);
    let scope = Scope::default().reading_bodies(&types);
    let report =
        anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([9; 32]), &scope)
            .expect("anonymises");
    assert_eq!(report.redacted_parts, 1);
    let wire = wire(&out);
    assert!(
        !wire.contains('\u{1}') && wire.contains("TRANSFER") && !wire.contains("61116666"),
        "{wire}"
    );
}

#[test]
fn a_short_payload_never_rewrites_or_hides_the_digits_it_spells_elsewhere() {
    let extra = "User-to-User: 12;encoding=hex\r\nX-Num: 412345678\r\nX-Intl: 002862345678\r\n\
                 X-Short: 51234\r\n";
    let raws = vec![invite("c1", "f1", "t1", "b1", extra, None)];
    let (raw, out, report) = anonymised(&raws);
    let text = wire(&out);
    assert!(!text.contains("412345678") && !text.contains("62345678"), "{text}");
    assert!(text.contains("X-Short: 51234"), "a short value is neither garbled nor mapped: {text}");
    assert_eq!(verdict(&raw, &out, &report), 0, "{text}");
}
