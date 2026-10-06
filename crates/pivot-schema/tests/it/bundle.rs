//! Run-bundle fixture conformance: every shipped record parses through the
//! typed model and re-serializes BYTE-IDENTICALLY in the byte form its file
//! carries — the canonical document form for the four `*.json` kinds, the
//! canonical line form for the `.jsonl` ladder.
//!
//! These bytes are the reference `@sip/contracts` decodes and re-emits, exactly
//! as the pivot fixtures are for the document. They are hand-composed rather
//! than cut from a run, so between them they hold what one run never does: every
//! failure the verdict can state, both shapes of a `must_fail` note, both
//! shapes of the RFC audit, a green run's four listings and an abandoned
//! script's close.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pivot_schema::bundle::{RecordedMessage, RunConfig, RunRfcAudit, RunTiming, RunVerdict};
use pivot_schema::canonical;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle")
}

/// Every fixture, by file name, with its text.
fn fixtures() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(fixture_dir())
        .expect("the fixture directory is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .map(|path| {
            let name = path.file_name().expect("a named file").to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&path).expect("a readable fixture"))
        })
        .collect();
    out.sort();
    assert_eq!(out.len(), 8, "the fixture set changed size");
    out
}

fn verdicts() -> Vec<(String, RunVerdict)> {
    fixtures()
        .into_iter()
        .filter(|(name, _)| name.starts_with("verdict-"))
        .map(|(name, text)| {
            let verdict = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, verdict)
        })
        .collect()
}

/// Re-serialize `text` through `T` in the canonical document form.
fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(name: &str, text: &str) -> String {
    let value: T = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    canonical::format(&value).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The record kind a fixture's name declares. Nothing is inferred from content:
/// a bundle names its files, and so does this set.
fn re_serialized(name: &str, text: &str) -> String {
    match name {
        "run-config.json" => round_trip::<RunConfig>(name, text),
        "timing.json" => round_trip::<RunTiming>(name, text),
        "rfc.json" | "rfc-not-audited.json" => round_trip::<RunRfcAudit>(name, text),
        _ if name.starts_with("verdict-") => round_trip::<RunVerdict>(name, text),
        // A ladder is a stream: each line is one record, and the file's own
        // layout is the newlines between them.
        _ if name.ends_with(".jsonl") => text
            .lines()
            .map(|line| {
                let message: RecordedMessage =
                    serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}: {e}"));
                canonical::format_line(&message).expect("a recorded message serializes") + "\n"
            })
            .collect(),
        other => panic!("{other}: the fixture set holds no such record kind"),
    }
}

#[test]
fn every_fixture_round_trips_byte_identically() {
    for (name, text) in fixtures() {
        assert_eq!(re_serialized(&name, &text), text, "{name}: re-serialized bytes differ");
    }
}

#[test]
fn re_parsing_a_re_serialized_fixture_yields_the_same_record() {
    for (name, verdict) in verdicts() {
        let text = canonical::format(&verdict).unwrap();
        let twice: RunVerdict =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(verdict, twice, "{name}");
    }
}

/// §2.2, the same rule the document obeys: no field's emptiness carries
/// meaning, so an empty collection is never written. A regression here would be
/// silent — the record still parses.
#[test]
fn no_fixture_writes_an_empty_collection() {
    for (name, text) in fixtures() {
        // A ladder holds one value per line; every other kind is one value.
        let records: Vec<&str> =
            if name.ends_with(".jsonl") { text.lines().collect() } else { vec![&text] };
        for record in records {
            let value: serde_json::Value =
                serde_json::from_str(record).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(empty_paths(&value, "").is_empty(), "{name}: {:?}", empty_paths(&value, ""));
        }
    }
}

fn empty_paths(value: &serde_json::Value, at: &str) -> Vec<String> {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => vec![at.to_string()],
        serde_json::Value::Array(items) if items.is_empty() => vec![at.to_string()],
        serde_json::Value::Object(map) => {
            map.iter().flat_map(|(k, v)| empty_paths(v, &format!("{at}.{k}"))).collect()
        }
        serde_json::Value::Array(items) => {
            items.iter().flat_map(|v| empty_paths(v, &format!("{at}[]"))).collect()
        }
        _ => Vec::new(),
    }
}

/// Every failure the verdict can state appears in the set, read off the EXPORTED
/// schema rather than a hand-kept list: a new `Failure` variant grows the
/// schema's `oneOf` and fails here until a fixture carries it, which is the
/// point — a mirror that has never seen a variant's bytes has not been checked
/// against it.
#[test]
fn the_fixture_set_carries_every_failure_the_verdict_can_state() {
    let schema = serde_json::to_value(schemars::schema_for!(RunVerdict)).expect("a schema");
    let declared: BTreeSet<String> = schema["$defs"]["Failure"]["oneOf"]
        .as_array()
        .expect("`Failure` is a tagged union of its variants")
        .iter()
        .map(|variant| {
            variant["properties"]["failure"]["const"]
                .as_str()
                .expect("every variant pins its `failure` tag")
                .to_string()
        })
        .collect();
    assert!(declared.len() > 20, "the schema no longer enumerates the failures: {declared:?}");

    // Read off the PARSED verdicts, at every place a failure may sit: a text
    // search would also see the `must_fail` vocabulary, which is a different
    // enum that happens to share the key.
    let mut carried: BTreeSet<String> = BTreeSet::new();
    for (name, verdict) in verdicts() {
        let sites = verdict
            .failures
            .iter()
            .chain(&verdict.tolerated)
            .chain(verdict.informative.iter().map(|note| &note.finding))
            .chain(verdict.must_fail.iter().filter_map(|note| note.observed.as_ref()));
        for failure in sites {
            let tag = serde_json::to_value(failure).unwrap_or_else(|e| panic!("{name}: {e}"));
            carried.insert(tag["failure"].as_str().expect("a tagged failure").to_string());
        }
    }
    assert_eq!(
        declared.difference(&carried).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "no fixture carries these failures"
    );
}

/// Each record kind refuses a field it does not know, in the SCHEMA and not only
/// in serde: a mirror validating against the export must refuse the same typo
/// Rust does. A kind written as a tagged union states it once per variant, and
/// every one of them has to.
#[test]
fn every_exported_bundle_schema_refuses_an_unknown_field() {
    for (kind, schema) in [
        ("run-config", serde_json::to_value(schemars::schema_for!(RunConfig)).unwrap()),
        ("verdict", serde_json::to_value(schemars::schema_for!(RunVerdict)).unwrap()),
        ("timing", serde_json::to_value(schemars::schema_for!(RunTiming)).unwrap()),
        ("recording", serde_json::to_value(schemars::schema_for!(RecordedMessage)).unwrap()),
        ("rfc", serde_json::to_value(schemars::schema_for!(RunRfcAudit)).unwrap()),
    ] {
        let objects: Vec<&serde_json::Value> = match schema["oneOf"].as_array() {
            Some(variants) => variants.iter().collect(),
            None => vec![&schema],
        };
        assert!(!objects.is_empty(), "{kind}: the schema declares no object");
        for object in objects {
            assert_eq!(object["additionalProperties"], serde_json::json!(false), "{kind}");
        }
    }
}

/// The two timing types the bundle and the document each carry are DIFFERENT
/// facts — one run's clock against the budgets a document declares — and neither
/// accepts the other's bytes. Two names are what a mirror reads them apart by;
/// this is what makes reading one as the other fail loudly.
#[test]
fn the_run_s_clock_and_the_document_s_budgets_are_not_interchangeable() {
    let (_, run) = fixtures()
        .into_iter()
        .find(|(name, _)| name == "timing.json")
        .expect("the run timing fixture");
    assert!(serde_json::from_str::<RunTiming>(&run).is_ok());
    assert!(
        serde_json::from_str::<pivot_schema::document::Timing>(&run).is_err(),
        "a run's clock read as the document's budgets"
    );

    let document = r#"{"expect_budget_ms":8000,"settle_budget_ms":32000}"#;
    assert!(serde_json::from_str::<pivot_schema::document::Timing>(document).is_ok());
    assert!(
        serde_json::from_str::<RunTiming>(document).is_err(),
        "the document's budgets read as a run's clock"
    );
}
