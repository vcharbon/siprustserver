//! The binary's seed and check contracts, end to end over a reference
//! capture: `auto` refuses to run without a seed it can keep, keeps a fresh
//! one, prints it nowhere, and reproduces its output byte for byte under the
//! kept seed; `anonymize --check-reproducible` answers on a raw document and
//! refuses an anonymised one. Over the crafted document: which addresses,
//! texts and body parts each scope keeps.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::support::{
    arg, capture, crafted, data, dir, extract, plan_args, profile_args, run, said, KEPT_TEXT,
    ORDER, PUBLIC_PEER,
};

/// The basic call's raw document under `root`.
fn basic(root: &Path) -> PathBuf {
    extract(&capture("basic-call.pcapng"), root)
}

/// `auto` of `flows` into `out` as capture `basic`, `--verify`, under the
/// test plan, profile and reading, `extra` added.
fn auto(flows: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["auto".to_string(), arg(flows), arg(out)];
    args.extend(["--capture", "basic", "--verify"].map(String::from));
    args.extend(plan_args());
    args.extend(profile_args());
    args.extend(extra.iter().map(|a| a.to_string()));
    run(&args)
}

/// The plan, the profile and the reading are the caller's: `auto` holds no
/// default for any of them and refuses to run without each.
#[test]
fn auto_refuses_to_run_without_its_plan_profile_or_reading() {
    let root = dir("no-pack");
    let flows = basic(&root);
    let seed = root.join("basic.seed");
    let all: Vec<String> = plan_args().into_iter().chain(profile_args()).collect();
    for missing in ["--plan", "--profile", "--reading"] {
        let mut args = vec!["auto".to_string(), arg(&flows), arg(&root.join("out"))];
        args.extend(["--capture".to_string(), "basic".to_string()]);
        for pair in all.chunks(2).filter(|p| p[0] != missing) {
            args.extend_from_slice(pair);
        }
        args.extend(["--seed-file".to_string(), arg(&seed), "--fresh-seed".to_string()]);
        let out = run(&args);
        assert!(!out.status.success(), "{missing}: {}", said(&out));
        assert!(said(&out).contains(missing), "{missing}: {}", said(&out));
        assert!(!root.join("out/basic.anon.flows.json").exists());
    }
}

#[test]
fn auto_refuses_to_run_without_a_seed() {
    let root = dir("no-seed");
    let out = auto(&basic(&root), &root.join("out"), &[]);
    assert!(!out.status.success());
    assert!(said(&out).contains("--seed-file"), "{}", said(&out));
    assert!(!root.join("out/basic.anon.flows.json").exists());
}

#[test]
fn a_fresh_seed_is_kept_unprinted_and_the_kept_seed_reproduces_the_output() {
    let root = dir("kept");
    let flows = basic(&root);
    let seed = arg(&root.join("basic.seed"));
    let first = auto(&flows, &root.join("one"), &["--seed-file", &seed, "--fresh-seed"]);
    assert!(first.status.success(), "{}", said(&first));
    let kept = std::fs::read_to_string(&seed).expect("the seed is kept");
    assert_eq!(std::fs::metadata(&seed).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!said(&first).contains(kept.trim()), "the seed is printed nowhere");
    let json = std::fs::read_to_string(root.join("one/auto.json")).expect("a summary");
    assert!(!json.contains(kept.trim()), "the seed stays out of the summary");

    let again = auto(&flows, &root.join("two"), &["--seed-file", &seed]);
    assert!(again.status.success(), "{}", said(&again));
    for file in ["basic.anon.flows.json", "auto.json", "source.txt"] {
        assert_eq!(
            std::fs::read(root.join("one").join(file)).unwrap(),
            std::fs::read(root.join("two").join(file)).unwrap(),
            "{file} is byte-identical under the kept seed"
        );
    }
    let refused = auto(&flows, &root.join("three"), &["--seed-file", &seed, "--fresh-seed"]);
    assert!(!refused.status.success(), "a second fresh seed is a new capture name");
}

#[test]
fn auto_writes_the_document_the_callflow_and_the_summary_alone() {
    let root = dir("outputs");
    let seed = arg(&root.join("basic.seed"));
    let out = auto(&basic(&root), &root.join("out"), &["--seed-file", &seed, "--fresh-seed"]);
    assert!(out.status.success(), "{}", said(&out));
    let mut written: Vec<String> = std::fs::read_dir(root.join("out"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    assert_eq!(written, ["auto.json", "basic.anon.flows.json", "source.txt"]);
}

#[test]
fn a_seed_is_taken_from_a_file_never_from_the_command_line() {
    let root = dir("argv");
    let out = auto(&basic(&root), &root.join("out"), &["--seed", &"01".repeat(32)]);
    assert!(!out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("--seed"), "{}", said(&out));
}

#[test]
fn check_reproducible_holds_on_a_raw_document_and_refuses_an_anonymised_one() {
    let root = dir("check");
    let flows = basic(&root);
    let mut args = vec!["anonymize".to_string(), arg(&flows), "--check-reproducible".into()];
    args.extend(["--sample".to_string(), "2".to_string()]);
    args.extend(plan_args());
    let out = run(&args);
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("differ in 0 datagrams; 0 survivors"), "{}", said(&out));
    assert!(said(&out).contains("0 tokens no fake explains"), "{}", said(&out));
    let seed = arg(&root.join("basic.seed"));
    let anonymised = root.join("basic.anon.flows.json");
    let mut args = vec!["anonymize".to_string(), arg(&flows), arg(&anonymised)];
    args.extend(["--seed-file".to_string(), seed.clone(), "--fresh-seed".to_string()]);
    args.extend(plan_args());
    assert!(run(&args).status.success());
    let mut args = vec!["anonymize".to_string(), arg(&anonymised), "--check-reproducible".into()];
    args.extend(plan_args());
    let out = run(&args);
    assert!(!out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("already anonymised"), "{}", said(&out));
    let mut args = vec!["auto".to_string(), arg(&anonymised), arg(&root.join("auto"))];
    args.extend(["--seed-file".to_string(), seed]);
    args.extend(plan_args());
    args.extend(profile_args());
    let out = run(&args);
    assert!(!out.status.success(), "auto refuses an anonymised document: {}", said(&out));
    assert!(said(&out).contains("already anonymised"), "{}", said(&out));
}

/// `anonymize --verify` of `flows` into `out` under a fresh seed at `seed`,
/// `extra` added.
fn anonymize(flows: &Path, out: &Path, seed: &Path, extra: &[String]) -> Output {
    let mut args = vec!["anonymize".to_string(), arg(flows), arg(out)];
    args.extend(["--seed-file".to_string(), arg(seed), "--fresh-seed".into(), "--verify".into()]);
    args.extend(plan_args());
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn keep_addresses_keeps_a_public_peer_the_profile_scope_fakes_and_still_verifies() {
    let root = dir("keep-addresses");
    let flows = crafted(&root);
    let kept = root.join("kept.flows.json");
    let mut extra = profile_args();
    extra.push("--keep-addresses".into());
    let out = anonymize(&flows, &kept, &root.join("kept.seed"), &extra);
    assert!(out.status.success(), "{}", said(&out));
    assert!(std::fs::read_to_string(&kept).unwrap().contains(PUBLIC_PEER), "every address kept");

    let sut = root.join("sut.flows.json");
    let out = anonymize(&flows, &sut, &root.join("sut.seed"), &profile_args());
    assert!(out.status.success(), "{}", said(&out));
    let text = std::fs::read_to_string(&sut).unwrap();
    assert!(!text.contains(PUBLIC_PEER), "the SUT scope fakes the public peer");
    assert!(text.contains("10.0.0.1:5060"), "and keeps the SUT");

    let both = ["--all-addresses".to_string(), "--keep-addresses".to_string()];
    let both = anonymize(&flows, &root.join("x.flows.json"), &root.join("x.seed"), &both);
    assert!(!both.status.success(), "keeping and faking every address at once is refused");
}

#[test]
fn auto_as_is_over_an_anonymised_document_lands_what_auto_lands_from_the_raw_one() {
    let root = dir("as-is");
    let flows = basic(&root);
    let seed = root.join("basic.seed");
    let anonymised = root.join("basic.anon.flows.json");
    assert!(anonymize(&flows, &anonymised, &seed, &profile_args()).status.success());
    let from_raw = auto(&flows, &root.join("raw"), &["--seed-file", &arg(&seed)]);
    assert!(from_raw.status.success(), "{}", said(&from_raw));

    let as_is = root.join("as-is");
    let mut args = vec!["auto".to_string(), arg(&anonymised), arg(&as_is)];
    args.extend(["--capture".to_string(), "basic".to_string()]);
    args.extend(plan_args());
    args.extend(profile_args());
    args.push("--as-is".into());
    let out = run(&args);
    assert!(out.status.success(), "{}", said(&out));
    for file in ["basic.anon.flows.json", "source.txt"] {
        assert_eq!(
            std::fs::read(root.join("raw").join(file)).unwrap(),
            std::fs::read(as_is.join(file)).unwrap(),
            "{file}: the document taken as is is the one auto anonymises from the raw under its seed"
        );
    }
    let summary = |dir: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join("auto.json")).unwrap()).unwrap()
    };
    assert_eq!(
        summary(&as_is),
        serde_json::json!({ "capture": "basic" }),
        "landed, nothing mapped"
    );
    assert_eq!(summary(&root.join("raw"))["capture"], "basic");
    assert!(summary(&root.join("raw")).get("error").is_none(), "{}", summary(&root.join("raw")));

    let mut seeded = args.to_vec();
    seeded.extend(["--seed-file".to_string(), arg(&seed)]);
    assert!(!run(&seeded).status.success(), "a document taken as is draws on no seed");
}

#[test]
fn auto_as_is_over_a_raw_document_keeps_it_unanonymised() {
    let root = dir("as-is-raw");
    let flows = crafted(&root);
    let out_dir = root.join("out");
    let mut args = vec!["auto".to_string(), arg(&flows), arg(&out_dir)];
    args.extend(["--capture".to_string(), "crafted".to_string()]);
    args.extend(plan_args());
    args.extend(profile_args());
    args.push("--as-is".into());
    let out = run(&args);
    assert!(out.status.success(), "{}", said(&out));
    let kept = std::fs::read_to_string(out_dir.join("crafted.anon.flows.json")).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&flows).unwrap()).unwrap();
    let kept_doc: serde_json::Value = serde_json::from_str(&kept).unwrap();
    assert_eq!(raw["legs"], kept_doc["legs"]);
    assert!(kept.contains(PUBLIC_PEER));
}

/// A profile's kept texts stay as written under `--profile` and map under
/// `--profile --all-addresses`: a document that leaves the network keeps
/// none of the SUT's own strings.
#[test]
fn a_kept_text_stays_under_the_profile_and_maps_when_every_address_is_mapped() {
    let root = dir("kept-text");
    let flows = crafted(&root);
    let anonymised = |name: &str, extra: &[&str]| {
        let out = root.join(format!("{name}.flows.json"));
        let mut args = profile_args();
        args.extend(extra.iter().map(|a| a.to_string()));
        let said_out = anonymize(&flows, &out, &root.join(format!("{name}.seed")), &args);
        assert!(said_out.status.success(), "{}", said(&said_out));
        std::fs::read_to_string(out).unwrap()
    };
    let line = format!("User-Agent: {KEPT_TEXT}");
    assert!(std::fs::read_to_string(&flows).unwrap().contains(&line));
    assert!(anonymised("kept", &[]).contains(&line), "kept under the profile");
    assert!(!anonymised("mapped", &["--all-addresses"]).contains(KEPT_TEXT), "mapped");
}

/// How many body parts an `anonymize` run said it redacted.
fn redacted_parts(out: &Output) -> usize {
    let said = said(out);
    let before = said.split(" body parts redacted").next().expect("the run reports its count");
    before.rsplit(' ').next().and_then(|n| n.parse().ok()).expect("a count")
}

/// The profile's body types are read at every scope: a document that leaves
/// the network (`--all-addresses`) under the profile has the parts of the
/// types it states read, where without it they are redacted whole.
#[test]
fn the_profile_body_types_are_read_when_every_address_is_mapped() {
    let root = dir("body-types");
    let flows = crafted(&root);
    let anonymised = |name: &str, profile: &str| {
        let out = root.join(format!("{name}.flows.json"));
        let args = ["--all-addresses".to_string(), "--profile".to_string(), data(profile)];
        let said_out = anonymize(&flows, &out, &root.join(format!("{name}.seed")), &args);
        assert!(said_out.status.success(), "{}", said(&said_out));
        (redacted_parts(&said_out), std::fs::read_to_string(out).unwrap())
    };
    let (stated, read) = anonymised("stated", "test.profile.json");
    let (unstated, redacted) = anonymised("unstated", "samples.profile.json");
    assert_eq!((stated, unstated), (0, 1), "the order part is redacted without its type");
    assert!(read.contains("order ") && !read.contains(ORDER), "read: its number mapped");
    assert!(!redacted.contains("order "), "redacted whole");
}
