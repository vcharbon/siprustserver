//! The reference pack's captures through the binary: each is extracted as
//! `sipflow --json` extracts it, anonymised under a fresh seed with the leak
//! checks on (`--verify`), and anonymised again under the kept seed: the two
//! documents are byte-identical, they keep the raw document's legs and the
//! messages of each, and no identity the raw document holds (its numbers, its
//! names, its host names, its public addresses) is left in them. The profile
//! states each capture's own B2BUA address as the SUT
//! (`tests/data/samples.profile.json`).

use std::net::IpAddr;

use serde_json::Value;

use crate::identities::{holds_word, of_document, strings};
use crate::support::{arg, capture, data, dir, extract, plan_args, run, said};

/// The SUT addresses of the samples' profile.
fn sut() -> Vec<IpAddr> {
    let profile: Value = serde_json::from_str(
        &std::fs::read_to_string(data("samples.profile.json")).expect("the profile"),
    )
    .expect("the profile is JSON");
    profile["sut"]
        .as_array()
        .expect("a sut list")
        .iter()
        .map(|a| a.as_str().expect("an address").parse().expect("an address"))
        .collect()
}

/// The capture's raw document and its anonymised one, the latter written
/// twice under one seed and asserted byte-identical.
fn anonymised_twice(name: &str) -> (Value, Value) {
    let root = dir(&format!("sample-{}", name.replace('/', "-")));
    let raw = extract(&capture(name), &root);
    let seed = root.join("capture.seed");
    let (one, two) = (root.join("one.flows.json"), root.join("two.flows.json"));
    let anonymise = |out: &std::path::Path, fresh: bool| {
        let mut args = vec![
            "anonymize".to_string(),
            arg(&raw),
            arg(out),
            "--verify".into(),
            "--seed-file".into(),
            arg(&seed),
            "--profile".into(),
            data("samples.profile.json"),
        ];
        args.extend(plan_args());
        if fresh {
            args.push("--fresh-seed".into());
        }
        let out = run(&args);
        assert!(out.status.success(), "{name}: {}", said(&out));
    };
    anonymise(&one, true);
    anonymise(&two, false);
    let first = std::fs::read(&one).expect("the first output");
    assert_eq!(first, std::fs::read(&two).expect("the second output"), "{name}: the kept seed");
    let read = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).expect("a JSON document");
    (read(&std::fs::read(&raw).expect("the raw document")), read(&first))
}

/// The number of messages of each leg.
fn shape(doc: &Value) -> Vec<usize> {
    doc["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .map(|leg| leg["msgs"].as_array().map_or(0, Vec::len))
        .collect()
}

/// Anonymises the capture and asserts the output keeps its legs and messages
/// and none of the identities the raw document holds.
fn anonymises_whole(name: &str) {
    let (raw, out) = anonymised_twice(name);
    let raw_shape = shape(&raw);
    assert!(!raw_shape.is_empty() && raw_shape.iter().all(|&n| n > 0), "{name}: {raw_shape:?}");
    assert_eq!(shape(&out), raw_shape, "{name}: the legs and their messages");
    let identities = of_document(&raw, &sut());
    assert!(!identities.is_empty(), "{name}: the raw document holds identities");
    let raw_text = strings(&raw);
    assert!(identities.iter().all(|i| holds_word(&raw_text, i)), "{name}: {identities:?}");
    let out_text = strings(&out);
    let left: Vec<&String> = identities.iter().filter(|i| holds_word(&out_text, i)).collect();
    assert!(left.is_empty(), "{name}: {left:?} of {identities:?} survived");
}

#[test]
fn the_basic_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("basic-call.pcapng");
}

#[test]
fn the_emergency_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("emergency-call.pcapng");
}

#[test]
fn the_forked_reliable_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("forked-reliable.pcapng");
}

#[test]
fn the_reinvite_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("reinvite-call.pcapng");
}

#[test]
fn the_third_party_cross_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("third-party/asterisk-22-xcid.pcap");
}

#[test]
fn the_third_party_transfer_sample_anonymises_reproducibly_and_leaks_nothing() {
    anonymises_whole("third-party/refer-transfer.pcap");
}
