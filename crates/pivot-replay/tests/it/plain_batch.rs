//! `pivot-replay batch` with nothing registered: the batch process of a hosted
//! lane against a B2BUA on real loopback UDP whose default destination answers
//! nothing, so a call reaches its callee only where its dial's `X-Api-Call`
//! pins the b-leg to the cell's egress. Each fixture cell passes, and the
//! batch's end answers every cell's `cleanup` line off the B2BUA's
//! `b2bua_active_calls`, served here over HTTP as a hosted system publishes it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::B2buaSut;
use layer_harness::TransportKind;
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};

fn fixture(cell: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cells").join(cell)
}

/// A scratch directory for `run`, emptied.
fn scratch(run: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pivot-replay-batch-{run}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// A loopback UDP address no socket held a moment ago.
fn free_udp_port() -> SocketAddr {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a probe socket");
    socket.local_addr().expect("its address")
}

/// Serves `held()` as `b2bua_active_calls` on an HTTP listener of its own,
/// as a hosted system's `/metrics` does; its address.
fn metrics_of(held: Arc<dyn Fn() -> usize + Send + Sync>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a metrics listener");
    let addr = listener.local_addr().expect("its address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut head = [0u8; 512];
            let _ = stream.read(&mut head);
            let body = format!("b2bua_active_calls {}\n", held());
            let _ =
                write!(stream, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        }
    });
    addr
}

/// The run-spec of `cell` on the hosted lane `lane` at `ingress`, its bundle
/// under `out`, every endpoint on a loopback port of the OS's choosing.
fn run_spec(cell: &str, lane: &str, ingress: SocketAddr, out: &Path) -> serde_json::Value {
    serde_json::json!({
        "case": fixture(cell),
        "out_dir": out,
        "lane": { "kind": "hosted", "name": lane, "ingress": ingress.to_string(), "egress_endpoint": "callee" },
        "run": {
            "timing_tolerance_ms": 200,
            "endpoint_addresses": { "caller": "127.0.0.1:0", "callee": "127.0.0.1:0" }
        }
    })
}

/// The batch's protocol lines once it ran `cells` (each `(fixture, bundle)`)
/// on `lane`, the batch-spec stating `metrics`.
fn batch(
    dir: &Path,
    lane: &str,
    ingress: SocketAddr,
    metrics: SocketAddr,
    cells: &[(&str, PathBuf)],
) -> Vec<serde_json::Value> {
    let spec = dir.join("batch-spec.json");
    let batch =
        serde_json::json!({ "lane": lane, "metrics": metrics.to_string(), "work_dir": dir });
    std::fs::write(&spec, batch.to_string()).expect("the batch-spec");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pivot-replay"))
        .arg("batch")
        .arg(&spec)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the batch process");
    {
        let mut stdin = child.stdin.take().expect("its stdin");
        for (cell, out) in cells {
            let path = out.with_extension("spec.json");
            std::fs::write(&path, run_spec(cell, lane, ingress, out).to_string())
                .expect("a run-spec");
            writeln!(stdin, "{}", path.display()).expect("a cell line");
        }
    }
    let lines = BufReader::new(child.stdout.take().expect("its stdout"))
        .lines()
        .map(|line| serde_json::from_str(&line.expect("a line")).expect("a JSON line"))
        .collect();
    assert!(child.wait().expect("it exits").success());
    lines
}

/// Each verification line of the bundle at `out`, by name: its answer.
fn answers(out: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(out.join("verifications.json")).expect("the cell's lines");
    let lines: Vec<serde_json::Value> = serde_json::from_str(&text).expect("a list of lines");
    lines
        .iter()
        .map(|l| {
            (l["name"].as_str().unwrap().to_string(), l["answer"].as_str().unwrap().to_string())
        })
        .collect()
}

#[test]
fn each_call_reaches_its_cell_through_its_dial_and_the_batch_end_answers_cleanup() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let net: Arc<dyn SignalingNetwork> = Arc::new(RealSignalingNetwork::new());
    let h = Harness::with_network_and_clock(
        "plain-batch",
        net,
        Clock::system(),
        TransportKind::Live,
        Duration::from_secs(5),
    );
    let ingress = free_udp_port();
    let b2bua = rt.block_on(B2buaSut::route_api_call("127.0.0.1", 9).start(
        &h,
        "b2bua",
        &ingress.to_string(),
    ));
    let metrics = metrics_of(b2bua.active_calls_probe());
    let dir = scratch("hosted");
    let cells = [("alice-calls-bob", dir.join("basic")), ("bob-is-busy", dir.join("busy"))];
    let lines = rt.block_on(async {
        tokio::task::spawn_blocking({
            let (dir, addr, cells) = (dir.clone(), b2bua.addr, cells.clone());
            move || batch(&dir, "hosted", addr, metrics, &cells)
        })
        .await
        .expect("the batch ran")
    });

    assert!(lines[0]["ready"].is_object(), "the ready line first: {lines:?}");
    let ran: Vec<&serde_json::Value> = lines.iter().filter_map(|l| l.get("cell")).collect();
    assert_eq!(ran.len(), 2, "{lines:?}");
    for cell in &ran {
        assert_eq!(cell["exit"], 0, "{cell}");
    }
    let end = &lines.last().expect("the end line")["end"];
    assert_eq!(end["dirty"], serde_json::Value::Null, "{end}");
    assert_eq!(end["gates"][0]["name"], "cleanup");
    assert_eq!(end["gates"][0]["answer"], "pass", "{end}");
    for (_, out) in &cells {
        let lines = answers(out);
        assert!(lines.contains(&("cleanup".into(), "pass".into())), "{lines:?}");
        assert!(lines.contains(&("call".into(), "pass".into())), "{lines:?}");
    }
    drop(h);
}

#[test]
fn a_hosted_cell_run_on_its_own_is_refused_naming_the_batch() {
    let dir = scratch("alone");
    let out = dir.join("basic");
    let spec = out.with_extension("spec.json");
    let ingress: SocketAddr = "127.0.0.1:9".parse().unwrap();
    std::fs::write(&spec, run_spec("alice-calls-bob", "hosted", ingress, &out).to_string())
        .unwrap();
    let ran = Command::new(env!("CARGO_BIN_EXE_pivot-replay"))
        .arg(&spec)
        .output()
        .expect("the binary runs");
    assert_eq!(ran.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&ran.stderr).contains("pivot-replay batch"));
}
