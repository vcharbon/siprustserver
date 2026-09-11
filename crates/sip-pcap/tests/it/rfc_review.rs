//! The `sipflow --rfc` contract, end to end through the binary on a small
//! anonymized capture: the capture-stack duplicates collapse before anything
//! is judged, the selection is what is reviewed, every hit is placed on the
//! side the stated SUT set puts it, and the two outputs — the text a reader
//! gets and the report a consumer parses — are pinned as goldens.
//!
//! The fixture is one call of a production capture re-encoded from its
//! anonymized flows document (`sipflow --to-pcap`): every number, host, IP
//! and vendor string rewritten, seven INVITE-transaction datagrams written
//! twice 100 µs apart the way a mirrored tap does. Its known defect is the
//! one the census charged on the source: a dialog-creating 2xx nobody ACKs,
//! on both sides of the B2BUA.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rfc-review")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sipflow-rfc-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join(name)
}

/// Run the built `sipflow` on the fixture with `args`, returning stdout,
/// stderr and the exit status.
fn sipflow(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_sipflow"))
        .arg(fixtures().join("capture.pcap.gz"))
        .args(args)
        .output()
        .expect("sipflow runs");
    (
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
        out.status.success(),
    )
}

/// The golden at `name`, or — under `UPDATE_GOLDENS=1` — the text written
/// there, so a deliberate change to the output is one env var and a diff.
fn golden(name: &str, actual: &str) {
    let path = fixtures().join(name);
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, actual).expect("golden written");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("golden present");
    assert_eq!(actual, expected, "{} drifted (UPDATE_GOLDENS=1 to accept)", path.display());
}

#[test]
fn the_review_of_the_selection_is_placed_on_the_stated_side() {
    let json = scratch("review.json");
    let (stdout, stderr, ok) =
        sipflow(&["--sut", "192.0.2.10", "--rfc", "--rfc-json", json.to_str().unwrap()]);
    assert!(ok, "{stderr}");

    // The duplicates the tap wrote are collapsed at ingest, before any rule
    // reads the wire: 26 records, 7 of them copies, 19 SIP messages.
    assert!(stderr.contains("records=26 datagrams=26"), "{stderr}");
    assert!(stderr.contains("sip-messages=19 capture-dups=7 parse-failed=0"), "{stderr}");
    assert!(stderr.contains("legs=3 call-groups=1 matched=1"), "{stderr}");
    assert!(stderr.contains("rules=wire sut=192.0.2.10 (stated)"), "{stderr}");
    assert!(stderr.contains("no-ack-to-dialog-creating-2xx: 2 hit(s)"), "{stderr}");
    assert!(stderr.contains("side peer: 1\n  side platform: 1"), "{stderr}");

    golden("review.golden.txt", &stdout);

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("report")).expect("json");
    let sides: Vec<(&str, &str, &str)> = report["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| {
            (
                h["rule"].as_str().unwrap(),
                h["emitter"].as_str().unwrap(),
                h["side"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sides,
        vec![
            ("no-ack-to-dialog-creating-2xx", "198.51.100.2:5060", "peer"),
            ("no-ack-to-dialog-creating-2xx", "192.0.2.10:5072", "platform"),
        ]
    );
    assert_eq!(report["sut"]["decided_by"], "stated");
    assert_eq!(report["hits"][0]["capture"], "capture.pcap.gz");
    golden("review.golden.json", &std::fs::read_to_string(&json).expect("report"));
}

/// `auto` reads the platform off the derived-Call-ID evidence — the socket
/// that re-originated the call — and lands on the same set a human states.
#[test]
fn an_automatic_sut_set_is_the_mint_point() {
    let (_, stderr, ok) = sipflow(&["--sut", "auto", "--rfc", "--list"]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("sut=192.0.2.10 (mint-point)"), "{stderr}");
}

/// A stated set the capture never touched selects nothing: the SUT is a
/// selection filter before it is an attribution, and a review of no calls
/// reports no calls rather than reviewing a peer's traffic as the platform's.
#[test]
fn a_sut_the_capture_never_touched_selects_no_call() {
    let (stdout, stderr, ok) = sipflow(&["--sut", "203.0.113.1", "--rfc", "--list"]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("matched=0"), "{stderr}");
    assert!(stderr.contains("0 selected group(s)"), "{stderr}");
    assert_eq!(stdout, "");
}

/// Without a SUT set the report is the census's own shape: no side on any
/// hit, no `sut` block, the topology role alone — byte-compatible with what
/// `--rfc-census` sweeps produce.
#[test]
fn without_a_sut_the_report_carries_no_side() {
    let json = scratch("unattributed.json");
    let (_, stderr, ok) = sipflow(&["--rfc", "--rfc-json", json.to_str().unwrap()]);
    assert!(ok, "{stderr}");
    let text = std::fs::read_to_string(&json).expect("report");
    assert!(!text.contains("\"side\""), "{text}");
    assert!(!text.contains("\"sut\""), "{text}");
    assert!(!text.contains("by_side"), "{text}");
}

/// The rule words: `all` widens past the census vocabulary and the summary
/// then lists only the rules that had an occasion; an unknown token is a
/// refusal, not an empty review.
#[test]
fn the_rule_set_is_named_and_an_unknown_rule_is_refused() {
    let (_, stderr, ok) =
        sipflow(&["--sut", "192.0.2.10", "--rfc", "--rfc-rules", "all", "--list"]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("rules=all"), "{stderr}");
    assert!(stderr.contains("sdp-origin-continuity: 1 hit(s)"), "{stderr}");
    assert!(!stderr.contains("serial-register"), "a rule with no occasion is not listed\n{stderr}");

    let (_, stderr, ok) = sipflow(&["--rfc", "--rfc-rules", "no-such-rule"]);
    assert!(!ok);
    assert!(stderr.contains("unknown rule \"no-such-rule\""), "{stderr}");
}

/// The fixture is what `--to-pcap` writes: decoding it, emitting the document
/// and encoding that again reproduces the capture byte for byte, so the
/// encoder is pinned by the same file the review is.
#[test]
fn the_fixture_round_trips_through_the_encoder() {
    let doc = scratch("fixture.flows.json");
    let pcap = scratch("fixture.pcap");
    let out = Command::new(env!("CARGO_BIN_EXE_sipflow"))
        .arg(fixtures().join("capture.pcap.gz"))
        .arg("--json")
        .output()
        .expect("sipflow runs");
    assert!(out.status.success());
    std::fs::write(&doc, &out.stdout).expect("doc written");
    let out = Command::new(env!("CARGO_BIN_EXE_sipflow"))
        .args(["--to-pcap", doc.to_str().unwrap(), "--out", pcap.to_str().unwrap()])
        .output()
        .expect("sipflow runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let (datagrams, _) = sip_pcap::read_capture_files(&[pcap]).expect("re-read");
    // The document is the model — the 7 collapsed duplicates are not in it.
    assert_eq!(datagrams.len(), 19);
    let (original, _) =
        sip_pcap::read_capture_files(&[fixtures().join("capture.pcap.gz")]).expect("fixture");
    let flows = sip_pcap::flow::build_flows(&original, &sip_pcap::flow::FlowConfig::default());
    let kept: Vec<(u64, String, Vec<u8>)> = flows
        .legs
        .iter()
        .flat_map(|l| l.msgs.iter().map(|m| (m.ts_us, m.src.to_string(), m.raw().to_vec())))
        .collect();
    for d in &datagrams {
        assert!(
            kept.contains(&(d.ts_us, d.src.to_string(), d.payload.clone())),
            "re-encoded datagram at {} from {} is not one the fixture holds",
            d.ts_us,
            d.src
        );
    }
}
