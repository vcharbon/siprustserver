use std::io::BufRead;
use std::net::TcpListener;

use serde_json::json;

use super::*;

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("loadgen-outcome-{name}-{}", std::process::id()))
}

/// Every record written reaches the file by `close`, one JSON line each, and
/// is counted by stream, shape and ending; calls never placed are counted
/// apart.
#[test]
#[ignore = "slow lane: loadgen"]
fn records_reach_the_file_and_the_counters() {
    let dir = scratch("file");
    let file = dir.join("records.jsonl");
    let sink = OutcomeSink::new("r1", "loadgen", "main", &file, None).unwrap();
    for ending in ["answered", "answered", "rejected"] {
        sink.write(json!({ "shape": "basic_call", "ending": ending }));
    }
    sink.not_placed("basic_call", "key_in_flight");
    sink.close();
    let lines: Vec<String> = std::io::BufReader::new(std::fs::File::open(&file).unwrap())
        .lines()
        .map(Result::unwrap)
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(lines.len(), 3);
    assert_eq!((sink.records_written(), sink.not_placed_total()), (3, 1));
    let mut text = String::new();
    sink.render_prometheus(&mut text);
    assert!(text.contains(
        "loadgen_outcome_records_total{stream=\"main\",shape=\"basic_call\",ending=\"answered\"} 2"
    ), "{text}");
    assert!(text.contains(
        "loadgen_not_placed_total{stream=\"main\",shape=\"basic_call\",reason=\"key_in_flight\"} 1"
    ), "{text}");
}

/// With a VictoriaLogs target, the records go out as one jsonline POST whose
/// lines carry `_time` (the record's end), the record verbatim in `_msg`, and
/// its stream and grouping fields.
#[test]
#[ignore = "slow lane: loadgen"]
fn records_are_pushed_to_victorialogs() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = LogsTarget::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = vec![0; 64 * 1024];
        let mut got = Vec::new();
        loop {
            let n = conn.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if body.len() >= len {
                    conn.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
                    return (head.to_string(), body.to_string());
                }
            }
        }
    });
    let dir = scratch("push");
    let sink =
        OutcomeSink::new("r1", "loadgen", "main", &dir.join("r.jsonl"), Some(target)).unwrap();
    let record = json!({
        "run": "r1", "source": "loadgen", "stream": "main", "call_id": "c1",
        "shape": "basic_call", "ending": "answered", "ended_at_ms": 1234, "legs": [{ "role": "bob" }],
    });
    sink.write(record.clone());
    sink.close();
    let (head, body) = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        head.starts_with("POST /insert/jsonline?_stream_fields=run,source,stream HTTP/1.1"),
        "{head}"
    );
    assert!(head.contains("Content-Type: application/stream+json"), "{head}");
    let line: Value = serde_json::from_str(body.trim_end()).unwrap();
    assert_eq!(line["_time"], json!(1234));
    let shipped: Value = serde_json::from_str(line["_msg"].as_str().unwrap()).unwrap();
    assert_eq!(shipped, record, "the record travels whole in `_msg`");
    for field in ["run", "source", "stream", "shape", "ending", "call_id"] {
        assert_eq!(line[field], record[field], "{field} beside it");
    }
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_records_url_reads_its_host_and_base() {
    let t = LogsTarget::parse("http://172.20.0.1:9428").unwrap();
    assert_eq!(t.authority, "172.20.0.1:9428");
    assert_eq!(t.path, "/insert/jsonline?_stream_fields=run,source,stream");
    let t = LogsTarget::parse("http://logs:9428/vl/").unwrap();
    assert_eq!(t.path, "/vl/insert/jsonline?_stream_fields=run,source,stream");
    assert!(LogsTarget::parse("https://logs:9428").is_err());
}

/// An endpoint that never answers holds the writer for the push timeout, not
/// for the system's: `close` returns, and the file holds the record.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_silent_endpoint_does_not_hold_the_close() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = LogsTarget::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let dir = scratch("silent");
    let file = dir.join("r.jsonl");
    let sink = OutcomeSink::new("r1", "loadgen", "main", &file, Some(target)).unwrap();
    sink.write(json!({ "shape": "basic_call", "ending": "answered", "ended_at_ms": 1 }));
    let started = std::time::Instant::now();
    sink.close();
    let held = started.elapsed();
    let lines = std::fs::read_to_string(&file).unwrap().lines().count();
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(held < Duration::from_secs(4), "the close waited {held:?}");
    assert_eq!(lines, 1);
}

/// Thousands of records queued for an endpoint that accepts and never
/// answers: `close` returns within its bound, every record still reaches the
/// file, and each record the log store never got is counted dropped, with the
/// failed pushes counted.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_blackholed_endpoint_drops_and_counts_within_the_close_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = LogsTarget::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let dir = scratch("blackhole");
    let file = dir.join("r.jsonl");
    let sink = OutcomeSink::new("r1", "loadgen", "main", &file, Some(target)).unwrap();
    for i in 0..5000 {
        sink.write(json!({ "shape": "basic_call", "ending": "answered", "ended_at_ms": i }));
    }
    let started = std::time::Instant::now();
    sink.close();
    let held = started.elapsed();
    let lines = std::fs::read_to_string(&file).unwrap().lines().count();
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(held < Duration::from_secs(10), "the close waited {held:?}");
    assert_eq!(lines, 5000, "every record reaches the file");
    let mut text = String::new();
    sink.render_prometheus(&mut text);
    assert!(
        text.contains(
            "loadgen_outcome_records_dropped_total{stream=\"main\",stage=\"push\"} 5000\n"
        ),
        "{text}"
    );
    let failed: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("loadgen_outcome_records_push_failed_total{stream=\"main\"} "))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(failed >= 1, "{text}");
}

/// loadgen ships the one line shape every producer ships (ADR-0049 §4.4), the
/// one the tooling's shared fixture states and the SIPp record twin is tested
/// against: every record field at top level, the record whole in `_msg`.
#[test]
fn a_records_line_has_the_shared_shape() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tooling/hakit/tests/fixtures/shipped-record.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let record = fixture["record"].clone();
    let line: Value = serde_json::from_str(&logs_line(record.clone())).unwrap();
    let mut fields: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort_unstable();
    let want: Vec<&str> =
        fixture["fields"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    assert_eq!(fields, want);
    let shipped: Value = serde_json::from_str(line["_msg"].as_str().unwrap()).unwrap();
    assert_eq!(shipped, record, "the record travels whole in `_msg`");
    assert_eq!(line["_time"], record["ended_at_ms"]);
}
