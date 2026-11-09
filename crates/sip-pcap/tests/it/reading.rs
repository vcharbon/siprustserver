//! The `sipflow --reading` contract: the file states the exemptions and bounds
//! every message's bytes are parsed under (extraction from a capture, `--rfc`,
//! `--enrich`, `--reparse`), a shape it exempts is read where the RFC's reading
//! refuses it, and a file that does not read refuses the run naming each line.

use std::path::{Path, PathBuf};
use std::process::Command;

fn sipflow(args: &[&str]) -> (String, String, bool) {
    let out =
        Command::new(env!("CARGO_BIN_EXE_sipflow")).args(args).output().expect("sipflow runs");
    (
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
        out.status.success(),
    )
}

fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("sipflow-reading-{}-{test}", std::process::id()))
        .join("capture_1");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn utf8(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

/// The fixture's document with its first request's Max-Forwards removed
/// (RFC 3261 §8.1.1 makes it mandatory), written under `dir`.
fn document_without_max_forwards(dir: &Path) -> PathBuf {
    let capture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rfc-review/capture.pcap.gz");
    let (json, err, ok) = sipflow(&[utf8(&capture), "--json"]);
    assert!(ok, "the fixture emits: {err}");
    let mut doc: serde_json::Value = serde_json::from_str(&json).expect("document JSON");
    let raw = doc["legs"][0]["msgs"][0]["raw"].as_str().expect("text payload").to_string();
    let line = raw
        .split("\r\n")
        .find(|l| l.to_ascii_lowercase().starts_with("max-forwards:"))
        .expect("the request states Max-Forwards")
        .to_string();
    doc["legs"][0]["msgs"][0]["raw"] =
        serde_json::Value::String(raw.replacen(&format!("{line}\r\n"), "", 1));
    let path = dir.join("capture_1.anon.flows.json");
    std::fs::write(&path, doc.to_string()).expect("document");
    path
}

#[test]
fn a_stated_exemption_is_read_where_the_rfc_reading_refuses_it() {
    let dir = scratch("exempt");
    let document = document_without_max_forwards(&dir);
    let reading = dir.join("reading.conf");
    std::fs::write(&reading, "# the corpus holds it\nexemptions = max-forwards:absent\n")
        .expect("reading");

    let (_, plain, ok) = sipflow(&["--reparse", utf8(&dir)]);
    assert!(ok, "{plain}");
    let (_, stated, ok) = sipflow(&["--reading", utf8(&reading), "--reparse", utf8(&dir)]);
    assert!(ok, "{stated}");
    let (_, unread, enriched_plain) = sipflow(&["--enrich", utf8(&document)]);
    let (_, _, enriched) = sipflow(&["--reading", utf8(&reading), "--enrich", utf8(&document)]);
    std::fs::remove_dir_all(dir.parent().expect("scratch root")).ok();

    assert!(plain.contains("refused=1"), "the RFC reading refuses it: {plain}");
    assert!(stated.contains("refused=0"), "the stated reading reads it: {stated}");
    assert!(!enriched_plain, "the RFC reading cannot re-derive it: {unread}");
    assert!(enriched, "the stated reading re-derives it");
}

#[test]
fn a_reading_that_does_not_read_refuses_the_run_naming_each_line() {
    let dir = scratch("refused");
    let reading = dir.join("reading.conf");
    std::fs::write(&reading, "exemptions = not-an-exemption\nmax-datagram = 0\n").expect("reading");
    let (_, err, ok) = sipflow(&["--reading", utf8(&reading), "--reparse", utf8(&dir)]);
    std::fs::remove_dir_all(dir.parent().expect("scratch root")).ok();
    assert!(!ok, "refused");
    assert!(err.contains("line 1: exemptions"), "{err}");
    assert!(err.contains("line 2: max-datagram"), "{err}");
}

/// The fixture's document with two shapes RFC 3261 refuses and a reading may
/// exempt: the first INVITE with no Max-Forwards (§8.1.1), and the last leg's
/// BYE whose From URI carries `x-dup` twice with one value (§19.1.1). Written
/// back as a capture under `dir`.
fn capture_with_exempted_shapes(dir: &Path) -> PathBuf {
    let document = document_without_max_forwards(dir);
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&document).expect("document"))
            .expect("document JSON");
    let raw = doc["legs"][2]["msgs"][5]["raw"].as_str().expect("the BYE").to_string();
    assert!(raw.starts_with("BYE "), "{raw}");
    let from = raw.split("\r\n").find(|l| l.starts_with("From:")).expect("a From").to_string();
    let repeated = from.replacen('>', ";x-dup=1;x-dup=1>", 1);
    doc["legs"][2]["msgs"][5]["raw"] = serde_json::Value::String(raw.replacen(&from, &repeated, 1));
    std::fs::write(&document, doc.to_string()).expect("document");
    let capture = dir.join("exempted.pcap");
    let (_, err, ok) = sipflow(&["--to-pcap", utf8(&document), "--out", utf8(&capture)]);
    assert!(ok, "the document encodes as a capture: {err}");
    std::fs::remove_file(&document).expect("the document goes; the capture stays");
    capture
}

/// `(messages read, messages set aside)` of the extraction `args` runs.
fn extracted(args: &[&str]) -> (u64, u64) {
    let (json, err, ok) = sipflow(args);
    assert!(ok, "{err}");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("document JSON");
    let stats = &doc["flow_stats"];
    (stats["sip_messages"].as_u64().expect("read"), stats["parse_failed"].as_u64().expect("aside"))
}

/// The messages an `--rfc` review of `args` judged.
fn reviewed(dir: &Path, args: &[&str]) -> u64 {
    let report = dir.join("review.json");
    let mut args = args.to_vec();
    args.extend(["--rfc", "--rfc-json", utf8(&report)]);
    let (_, err, ok) = sipflow(&args);
    assert!(ok, "{err}");
    let review: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("the review"))
            .expect("review JSON");
    review["messages"].as_u64().expect("messages")
}

#[test]
fn extraction_and_review_read_what_the_reading_exempts() {
    let dir = scratch("extract");
    let capture = capture_with_exempted_shapes(&dir);
    let reading = dir.join("reading.conf");
    std::fs::write(&reading, "exemptions = max-forwards:absent,repeat:from.uri:x-dup\n")
        .expect("reading");
    let (capture, reading) = (utf8(&capture), utf8(&reading));

    let plain = extracted(&[capture, "--json"]);
    let stated = extracted(&["--reading", reading, capture, "--json"]);
    let plain_review = reviewed(&dir, &[capture]);
    let stated_review = reviewed(&dir, &["--reading", reading, capture]);
    std::fs::remove_dir_all(dir.parent().expect("scratch root")).ok();

    assert_eq!(plain, (17, 2), "the RFC reading sets both aside");
    assert_eq!(stated, (19, 0), "the stated reading reads both");
    assert_eq!((plain_review, stated_review), (17, 19), "the review judges what was read");
}
