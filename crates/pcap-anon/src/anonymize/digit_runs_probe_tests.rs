//! A body part of a type a profile states as digit runs: the anonymiser maps
//! its digit runs and keeps the rest as written, and verify reads the words
//! of that rest, reporting each one no fake explains unless the row's
//! vocabulary lists it, matched in any case. Every value is invented.

use serde_json::json;

use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};
use crate::profile::Profile;

const ORDER: &str = "application/vnd.example.order";

/// A scope reading [`ORDER`] as digit runs, under the row's `vocabulary`.
fn scope(vocabulary: &[&str]) -> Scope {
    let row = json!({ "media_type": ORDER, "read": "digit-runs", "vocabulary": vocabulary });
    Profile::from_json_str(&json!({ "sut": [], "bodies": [row] }).to_string())
        .expect("the profile reads")
        .scope()
}

/// One INFO carrying `body` as an [`ORDER`] part.
fn doc(body: &str) -> Flows {
    let uri = "sip:testo@order-host.example.com";
    let raw = format!(
        "INFO {uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKord1\r\n\
         From: <sip:caller@order-host.example.com>;tag=ordf\r\n\
         To: <{uri}>;tag=ordt\r\n\
         Call-ID: ord-call@order-host.example.com\r\nCSeq: 3 INFO\r\nMax-Forwards: 70\r\n\
         Content-Type: {ORDER}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "ord-call@order-host.example.com",
                   "hops": [{ "a": "192.0.2.10:5060", "b": "198.51.100.20:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "192.0.2.10:5060", "dst": "198.51.100.20:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "INFO", "uri": uri,
                                           "cseq": { "seq": 3, "method": "INFO" },
                                           "from": { "uri": "sip:caller@order-host.example.com", "tag": "ordf" },
                                           "to": { "uri": uri, "tag": "ordt" } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

/// The words verify reports in `flows` under `scope`, nothing anonymised.
fn reported(flows: &Flows, scope: &Scope) -> Vec<String> {
    crate::verify::leaks::unexplained(flows, scope, &Fakes::default(), &crate::testflow::reading())
        .into_iter()
        .filter(|(class, _)| *class == "word")
        .map(|(_, word)| String::from_utf8(word).expect("ASCII words"))
        .collect()
}

const BODY: &str = "ORDER=TRANSFER;TO=061116666;NOTE=Fakey McFakename;CODEC=G711\r\n";

#[test]
fn verify_reports_each_word_of_a_digit_run_body_its_vocabulary_does_not_list() {
    let raw = doc(BODY);
    let none = reported(&raw, &scope(&[]));
    for word in ["order", "transfer", "note", "codec", "fakey", "mcfakename"] {
        assert!(none.contains(&word.to_string()), "{word} is reported: {none:?}");
    }
    let listed = reported(&raw, &scope(&["Order", "TRANSFER", "to", "note", "codec"]));
    assert_eq!(listed, vec!["fakey", "mcfakename"], "the vocabulary matches in any case");
}

#[test]
fn a_digit_run_body_keeps_its_words_and_verify_counts_the_free_text_among_them() {
    let scope = scope(&["order", "transfer", "to", "note", "codec"]);
    for (body, words) in [(BODY, 2), ("ORDER=TRANSFER;TO=061116666\r\n", 0)] {
        let raw = doc(body);
        let mut out = raw.clone();
        let plan = crate::testflow::plan();
        let report =
            anonymize_in(&mut out, &plan, &Seed::from_bytes([4; 32]), &scope).expect("anonymises");
        let wire = String::from_utf8_lossy(&crate::model::wire(&out.legs[0].msgs[0])).into_owned();
        let written = wire.split("\r\n\r\n").nth(1).expect("a body");
        assert!(!written.contains("61116666"), "the number is mapped: {written}");
        let letters = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
        assert_eq!(letters(written), letters(body), "the rest is kept as written");
        let verdict = crate::verify::verdict::judge(
            &raw,
            &out,
            &plan,
            &scope,
            &crate::testflow::reading(),
            &report,
        );
        assert_eq!(verdict.leaks.words, words, "{body}");
    }
}

#[test]
fn a_vocabulary_is_words_of_letters_on_a_digit_run_row() {
    for row in [
        json!({ "media_type": ORDER, "read": "digit-runs", "vocabulary": ["to=", "order"] }),
        json!({ "media_type": ORDER, "read": "digit-runs", "vocabulary": ["g711"] }),
        json!({ "media_type": ORDER, "read": "digit-runs", "vocabulary": [""] }),
        json!({ "media_type": "application/vnd.example.ctl+xml", "read": "control",
                "vocabulary": ["play"] }),
    ] {
        let profile = json!({ "sut": [], "bodies": [row] }).to_string();
        assert!(Profile::from_json_str(&profile).is_err(), "{profile}");
    }
}
