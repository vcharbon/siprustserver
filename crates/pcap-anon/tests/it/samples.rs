//! The reference pack's captures through the binary: each is extracted as
//! `sipflow --json` extracts it, anonymised under a fresh seed with the leak
//! checks on (`--verify`), and anonymised again under the kept seed: the two
//! documents are byte-identical, and no number of the plan the capture
//! carries is left. The profile states each capture's own B2BUA address as
//! the SUT (`tests/data/samples.profile.json`).

use crate::support::{arg, capture, data, dir, extract, plan_args, run, said};

/// The capture's document anonymised twice under one seed; the first output.
fn anonymised_twice(name: &str) -> String {
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
    assert_ne!(first, std::fs::read(&raw).expect("the raw document"), "{name}: rewritten");
    String::from_utf8(first).expect("utf-8")
}

/// Asserts no number of the samples' dial plan survives in `text`.
fn no_sample_number(name: &str, text: &str) {
    for number in ["5555550100", "5555550101"] {
        assert!(!text.contains(number), "{name}: {number} survived");
    }
}

#[test]
fn the_basic_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "basic-call.pcapng";
    no_sample_number(name, &anonymised_twice(name));
}

#[test]
fn the_emergency_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "emergency-call.pcapng";
    no_sample_number(name, &anonymised_twice(name));
}

#[test]
fn the_forked_reliable_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "forked-reliable.pcapng";
    no_sample_number(name, &anonymised_twice(name));
}

#[test]
fn the_reinvite_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "reinvite-call.pcapng";
    no_sample_number(name, &anonymised_twice(name));
}

#[test]
fn the_third_party_cross_call_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "third-party/asterisk-22-xcid.pcap";
    no_sample_number(name, &anonymised_twice(name));
}

#[test]
fn the_third_party_transfer_sample_anonymises_reproducibly_and_leaks_nothing() {
    let name = "third-party/refer-transfer.pcap";
    no_sample_number(name, &anonymised_twice(name));
}
