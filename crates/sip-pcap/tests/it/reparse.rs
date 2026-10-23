//! The `sipflow --reparse` contract: every message of an emitted document goes
//! through the parser, each refusal is one JSON line naming its place and
//! reason, and the sweep continues past it to the totals.

use std::path::{Path, PathBuf};
use std::process::Command;

fn capture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rfc-review/capture.pcap.gz")
}

fn sipflow(args: &[&str]) -> (String, String, bool) {
    let out =
        Command::new(env!("CARGO_BIN_EXE_sipflow")).args(args).output().expect("sipflow runs");
    (
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
        out.status.success(),
    )
}

/// A document whose second message carries a header name that is no token
/// (RFC 3261 §25.1) is reported on that message alone, a third message whose
/// Refer-To is a bare addr-spec holding `?` (§20) is reported on that header
/// alone, and the sweep counts the rest.
#[test]
fn a_refused_message_is_one_line_and_the_sweep_continues() {
    let path = capture();
    let (json, _, ok) = sipflow(&[path.to_str().expect("utf-8 path"), "--json"]);
    assert!(ok, "the fixture emits");
    let mut doc: serde_json::Value = serde_json::from_str(&json).expect("document JSON");
    let total: usize = doc["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .map(|leg| leg["msgs"].as_array().expect("msgs").len())
        .sum();
    let raw = doc["legs"][0]["msgs"][1]["raw"].as_str().expect("text payload").to_string();
    let refused = raw.replacen("\r\n", "\r\nX(Bad): 1\r\n", 1);
    doc["legs"][0]["msgs"][1]["raw"] = serde_json::Value::String(refused);
    let raw = doc["legs"][0]["msgs"][2]["raw"].as_str().expect("text payload").to_string();
    let bare = "sip:bob@example.com?Subject=hi";
    let unread = raw.replacen("\r\n", &format!("\r\nRefer-To: {bare}\r\n"), 1);
    doc["legs"][0]["msgs"][2]["raw"] = serde_json::Value::String(unread);

    let dir = std::env::temp_dir()
        .join(format!("sipflow-reparse-{}", std::process::id()))
        .join("capture_1");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("capture_1.anon.flows.json"), doc.to_string()).expect("document");

    let (out, err, ok) = sipflow(&["--reparse", dir.to_str().expect("utf-8 path")]);
    std::fs::remove_dir_all(&dir).ok();
    assert!(ok, "the sweep exits cleanly: {err}");
    let lines: Vec<serde_json::Value> =
        out.lines().map(|l| serde_json::from_str(l).expect("one JSON line")).collect();
    assert_eq!(lines.len(), 2, "one refused message, one unread header: {out}");
    assert_eq!(lines[0]["capture"], "capture_1");
    assert_eq!((lines[0]["leg"].as_u64(), lines[0]["msg"].as_u64()), (Some(0), Some(1)));
    assert!(lines[0]["reason"].as_str().is_some_and(|r| r.contains("Header name")), "{}", lines[0]);
    assert_eq!((lines[1]["leg"].as_u64(), lines[1]["msg"].as_u64()), (Some(0), Some(2)));
    assert_eq!(lines[1]["header"], "Refer-To");
    assert_eq!(lines[1]["values"], serde_json::json!([bare]));
    assert!(err.contains(&format!("messages={total} refused=1")), "totals: {err}");
    assert!(err.contains("header-errors=1"), "totals: {err}");
}
