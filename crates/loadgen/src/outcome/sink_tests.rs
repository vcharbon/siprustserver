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
/// lines carry `_time` (the record's end) and `_msg`.
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
    sink.write(json!({ "shape": "basic_call", "ending": "answered", "ended_at_ms": 1234 }));
    sink.close();
    let (head, body) = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        head.starts_with("POST /insert/jsonline?_stream_fields=run,source,stream HTTP/1.1"),
        "{head}"
    );
    assert!(head.contains("Content-Type: application/stream+json"), "{head}");
    let line: Value = serde_json::from_str(body.trim_end()).unwrap();
    assert_eq!(
        (line["_time"].clone(), line["_msg"].clone()),
        (json!(1234), json!("basic_call answered"))
    );
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
