//! What the integration tests share: the invented plan, profile and reading
//! of `tests/data/`, the binary, a scratch directory per test, a reference
//! capture extracted as `sipflow --json` extracts it, and a crafted capture
//! document whose peer is public.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pcap_anon::plan::Plan;
use pcap_anon::reading::Reading;
use serde_json::{json, Value};
use sip_pcap::enrich::EnrichOptions;
use sip_pcap::flow::{build_flows, FlowConfig};

/// The path of a file of `tests/data/`.
pub fn data(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
        .to_str()
        .expect("utf-8")
        .to_string()
}

/// The invented numbering plan.
pub fn plan() -> Plan {
    Plan::from_json_str(&std::fs::read_to_string(data("test.plan.json")).expect("the plan file"))
        .expect("the plan reads")
}

/// The reading the tests extract and parse under.
pub fn reading() -> Reading {
    Reading::stated(&std::fs::read_to_string(data("test.reading")).expect("the file"), "reading")
        .expect("the reading reads")
}

/// `--plan` and `--reading` naming the test data.
pub fn plan_args() -> Vec<String> {
    vec!["--plan".into(), data("test.plan.json"), "--reading".into(), data("test.reading")]
}

/// `--profile` naming the invented profile.
pub fn profile_args() -> Vec<String> {
    vec!["--profile".into(), data("test.profile.json")]
}

/// A fresh directory for one test.
pub fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcap-anon-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp dir");
    dir
}

/// The binary run with `args`.
pub fn run<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pcap-anon")).args(args).output().expect("the binary runs")
}

/// What a run printed, both streams.
pub fn said(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// A path as an argument.
pub fn arg(path: &Path) -> String {
    path.to_str().expect("utf-8").to_string()
}

/// A capture of the reference pack (`ts/reference-pack/capture/`).
pub fn capture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ts/reference-pack/capture").join(name)
}

/// The flows document of `capture`, read as `sipflow --json` reads it under
/// the RFC's reading, written to `into/<stem>.flows.json`.
pub fn extract(capture: &Path, into: &Path) -> PathBuf {
    let (datagrams, stats) =
        sip_pcap::read_capture_set(&[capture], 60_000_000).expect("the capture reads");
    let sip = sip_message::ToolReading::plain_rfc();
    let read = build_flows(&datagrams, &FlowConfig::standard(sip.clone()));
    let doc = sip_pcap::emit::flows_to_doc(&read, &stats, &EnrichOptions::under(sip))
        .expect("the document emits");
    let stem = capture.file_name().and_then(|n| n.to_str()).expect("a name");
    let stem = stem.split('.').next().expect("a stem");
    let path = into.join(format!("{stem}.flows.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&doc).expect("serialises"))
        .expect("the document is written");
    path
}

/// The invented profile's SUT and the crafted document's public peer.
pub const SUT: &str = "10.0.0.1:5060";
pub const PUBLIC_PEER: &str = "8.8.8.10";

/// The text the invented profile keeps as written.
pub const KEPT_TEXT: &str = "Exampleswitch 4.2";

/// The order the crafted INVITE carries in a part of the profile's body type.
pub const ORDER: &str = "order 2861114444";

/// The crafted INVITE: the public peer calls through the SUT, with the
/// SUT's product string and a multipart body of SDP and an order part.
fn invite() -> String {
    let body = format!(
        "--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\no=- 1 1 IN IP4 {PUBLIC_PEER}\r\n\
         s=-\r\nc=IN IP4 {PUBLIC_PEER}\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n--b\r\n\
         Content-Type: application/vnd.example.order\r\n\r\n{ORDER}\r\n--b--\r\n"
    );
    format!(
        "INVITE sip:+2861113333@{SUT} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {PUBLIC_PEER}:5060;branch=z9hG4bKcrafted1\r\nMax-Forwards: 70\r\n\
         From: <sip:+2861112222@{PUBLIC_PEER}>;tag=f1\r\nTo: <sip:+2861113333@{SUT}>\r\n\
         Call-ID: crafted-1@{PUBLIC_PEER}\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:+2861112222@{PUBLIC_PEER}:5060>\r\nUser-Agent: {KEPT_TEXT}\r\n\
         Content-Type: multipart/mixed;boundary=b\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// One message of the crafted leg.
fn message(ms: u64, src: &str, dst: &str, raw: String, summary: Value) -> Value {
    json!({ "ts_us": ms * 1000, "src": src, "dst": dst, "hop": 0, "retx": false, "raw": raw,
            "summary": summary })
}

/// The crafted capture document: the INVITE and its 486, written to
/// `into/crafted.flows.json`.
pub fn crafted(into: &Path) -> PathBuf {
    let peer = format!("{PUBLIC_PEER}:5060");
    let from = json!({ "uri": format!("sip:+2861112222@{PUBLIC_PEER}"), "tag": "f1" });
    let to = |tag: Option<&str>| json!({ "uri": format!("sip:+2861113333@{SUT}"), "tag": tag });
    let busy = format!(
        "SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP {PUBLIC_PEER}:5060;branch=z9hG4bKcrafted1\r\n\
         From: <sip:+2861112222@{PUBLIC_PEER}>;tag=f1\r\nTo: <sip:+2861113333@{SUT}>;tag=t1\r\n\
         Call-ID: crafted-1@{PUBLIC_PEER}\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    let ack = format!(
        "ACK sip:+2861113333@{SUT} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {PUBLIC_PEER}:5060;branch=z9hG4bKcrafted1\r\nMax-Forwards: 70\r\n\
         From: <sip:+2861112222@{PUBLIC_PEER}>;tag=f1\r\nTo: <sip:+2861113333@{SUT}>;tag=t1\r\n\
         Call-ID: crafted-1@{PUBLIC_PEER}\r\nCSeq: 1 ACK\r\nContent-Length: 0\r\n\r\n"
    );
    let cseq = |method: &str| json!({ "seq": 1, "method": method });
    let msgs = vec![
        message(
            0,
            &peer,
            SUT,
            invite(),
            json!({ "kind": "request", "method": "INVITE",
            "uri": format!("sip:+2861113333@{SUT}"), "cseq": cseq("INVITE"), "from": from,
            "to": to(None) }),
        ),
        message(
            5,
            SUT,
            &peer,
            busy,
            json!({ "kind": "response", "status": 486,
            "reason": "Busy Here", "cseq": cseq("INVITE"), "from": from, "to": to(Some("t1")) }),
        ),
        message(
            6,
            &peer,
            SUT,
            ack,
            json!({ "kind": "request", "method": "ACK",
            "uri": format!("sip:+2861113333@{SUT}"), "cseq": cseq("ACK"), "from": from,
            "to": to(Some("t1")) }),
        ),
    ];
    let doc = json!({
        "schema": pcap_anon::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 3, "non_ip": 0, "non_udp": 0, "snap_truncated": 0,
                          "datagrams": 3, "fragments": 0, "reassembled": 0, "frag_dropped": 0,
                          "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 3, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": format!("crafted-1@{PUBLIC_PEER}"), "hops": [{ "a": peer, "b": SUT }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null,
                   "tokens": [], "msgs": msgs }],
        "groups": [{ "legs": [0], "evidence": [] }]
    });
    let flows = pcap_anon::model::ingest_str(&doc.to_string()).expect("the crafted document reads");
    let path = into.join("crafted.flows.json");
    std::fs::write(&path, pcap_anon::model::to_json_string(&flows, &reading()).expect("emits"))
        .expect("the document is written");
    path
}
