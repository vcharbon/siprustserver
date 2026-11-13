//! `pivot-replay batch` with nothing registered: the batch process of a hosted
//! lane against a B2BUA on real loopback UDP whose default destination answers
//! nothing, so a call reaches its callee only where its dial's `X-Api-Call`
//! pins the b-leg to the cell's egress. Each fixture cell passes, and the
//! batch's end answers every cell's `cleanup` line off the B2BUA's
//! `b2bua_active_calls`, `b2bua_store_calls` and `b2bua_store_sip_index`,
//! served here over HTTP as a hosted system publishes them.

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

pub(crate) fn fixture(cell: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cells").join(cell)
}

/// A scratch directory for `run`, emptied.
pub(crate) fn scratch(run: &str) -> PathBuf {
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

/// The Prometheus text a system's `/metrics` answers.
pub(crate) type Body = Arc<dyn Fn() -> String + Send + Sync>;

/// Serves `body()` on an HTTP listener of its own, as a hosted system's
/// `/metrics` does; its address.
pub(crate) fn metrics_of(body: Body) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a metrics listener");
    let addr = listener.local_addr().expect("its address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut head = [0u8; 512];
            let _ = stream.read(&mut head);
            let body = body();
            let _ =
                write!(stream, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        }
    });
    addr
}

/// A system's `/metrics` holding `active` calls in its gauge, `stored` in its
/// call map and `indexed` keys in its SIP routing index, whatever it ran.
fn gauges(active: u64, stored: u64, indexed: u64) -> Body {
    Arc::new(move || {
        format!(
            "b2bua_active_calls {active}\nb2bua_store_calls {stored}\n\
             b2bua_store_sip_index {indexed}\n"
        )
    })
}

/// The run-spec of `cell` on the hosted lane `lane` at `ingress`, its bundle
/// under `out`, every endpoint on a loopback port of the OS's choosing.
fn run_spec(cell: &str, lane: &str, ingress: SocketAddr, out: &Path) -> serde_json::Value {
    run_spec_at(&fixture(cell), lane, ingress, out)
}

/// [`run_spec`] of the case directory at `case`.
pub(crate) fn run_spec_at(
    case: &Path,
    lane: &str,
    ingress: SocketAddr,
    out: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "case": case,
        "out_dir": out,
        "lane": { "kind": "hosted", "name": lane, "ingress": ingress.to_string(), "egress_endpoint": "callee" },
        "run": {
            "timing_tolerance_ms": 200,
            "endpoint_addresses": { "caller": "127.0.0.1:0", "callee": "127.0.0.1:0" }
        }
    })
}

/// The batch's protocol lines once it ran `cells` (each `(fixture, bundle)`)
/// on `lane`, the batch-spec stating `metrics` and, where given, how long its
/// end waits for the system to hold no call.
fn batch(
    dir: &Path,
    lane: &str,
    ingress: SocketAddr,
    metrics: SocketAddr,
    reap_ms: Option<u64>,
    cells: &[(&str, PathBuf)],
) -> Vec<serde_json::Value> {
    let spec = dir.join("batch-spec.json");
    let mut batch =
        serde_json::json!({ "lane": lane, "metrics": metrics.to_string(), "work_dir": dir });
    if let Some(ms) = reap_ms {
        batch["metrics_reap_ms"] = ms.into();
    }
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
pub(crate) fn answers(out: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(out.join("verifications.json")).expect("the cell's lines");
    let lines: Vec<serde_json::Value> = serde_json::from_str(&text).expect("a list of lines");
    lines
        .iter()
        .map(|l| {
            (l["name"].as_str().unwrap().to_string(), l["answer"].as_str().unwrap().to_string())
        })
        .collect()
}

/// What a hosted batch of both fixture cells left: its protocol lines, each
/// cell's bundle and how long the batch process ran.
struct Ran {
    lines: Vec<serde_json::Value>,
    bundles: Vec<PathBuf>,
    took: Duration,
}

impl Ran {
    /// The end line's `cleanup` gate answer.
    fn cleanup(&self) -> &serde_json::Value {
        let end = &self.lines.last().expect("the end line")["end"];
        assert_eq!(end["gates"][0]["name"], "cleanup", "{end}");
        &end["gates"][0]["answer"]
    }

    /// Each cell's `cleanup` line answer.
    fn cells_cleanup(&self) -> Vec<String> {
        self.bundles
            .iter()
            .map(|out| {
                answers(out)
                    .into_iter()
                    .find(|(name, _)| name == "cleanup")
                    .map(|(_, answer)| answer)
                    .expect("a cleanup line")
            })
            .collect()
    }
}

/// A B2BUA on loopback UDP, on `rt`, whose default destination answers
/// nothing, and the harness it runs in.
pub(crate) fn stand_b2bua(rt: &tokio::runtime::Runtime) -> (Harness, B2buaSut) {
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
    (h, b2bua)
}

/// Both fixture cells run as one hosted batch against a B2BUA on loopback
/// UDP, its `/metrics` the body `metrics` makes of the B2BUA, the batch-spec
/// stating `reap_ms` where given.
fn hosted_batch(run: &str, metrics: impl FnOnce(&B2buaSut) -> Body, reap_ms: Option<u64>) -> Ran {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let (h, b2bua) = stand_b2bua(&rt);
    let metrics = metrics_of(metrics(&b2bua));
    let dir = scratch(run);
    let cells = [("alice-calls-bob", dir.join("basic")), ("bob-is-busy", dir.join("busy"))];
    let started = std::time::Instant::now();
    let lines = rt.block_on(async {
        tokio::task::spawn_blocking({
            let (dir, addr, cells) = (dir.clone(), b2bua.addr, cells.clone());
            move || batch(&dir, "hosted", addr, metrics, reap_ms, &cells)
        })
        .await
        .expect("the batch ran")
    });
    let took = started.elapsed();
    drop(h);
    Ran { lines, bundles: cells.into_iter().map(|(_, out)| out).collect(), took }
}

#[test]
fn each_call_reaches_its_cell_through_its_dial_and_the_batch_end_answers_cleanup() {
    let ran = hosted_batch(
        "hosted",
        |b2bua| {
            let metrics = b2bua.metrics().clone();
            Arc::new(move || metrics.prometheus_text())
        },
        None,
    );
    let lines = &ran.lines;

    assert!(lines[0]["ready"].is_object(), "the ready line first: {lines:?}");
    let cells: Vec<&serde_json::Value> = lines.iter().filter_map(|l| l.get("cell")).collect();
    assert_eq!(cells.len(), 2, "{lines:?}");
    for cell in &cells {
        assert_eq!(cell["exit"], 0, "{cell}");
    }
    let end = &lines.last().expect("the end line")["end"];
    assert_eq!(end["dirty"], serde_json::Value::Null, "{end}");
    assert_eq!(ran.cleanup(), "pass", "{end}");
    for out in &ran.bundles {
        let lines = answers(out);
        assert!(lines.contains(&("cleanup".into(), "pass".into())), "{lines:?}");
        assert!(lines.contains(&("call".into(), "pass".into())), "{lines:?}");
    }
}

/// A system whose gauge still holds a call once the batch's cells ended fails
/// the batch's cleanup gate and every placed cell's `cleanup` line, after the
/// wait the batch-spec states rather than a fixed one.
#[test]
fn a_call_the_system_still_holds_fails_every_cells_cleanup_after_the_specs_wait() {
    let ran = hosted_batch("held", |_| gauges(1, 0, 0), Some(200));

    assert_eq!(ran.cleanup(), "fail", "{:?}", ran.lines.last());
    assert_eq!(ran.cells_cleanup(), ["fail", "fail"]);
    assert!(ran.took < Duration::from_secs(5), "the end waited {:?}", ran.took);
}

/// A call left in the system's call map fails the cleanup gate though its
/// active-call gauge reads 0.
#[test]
fn a_call_left_in_the_call_map_fails_every_cells_cleanup_though_no_call_is_active() {
    let ran = hosted_batch("stored", |_| gauges(0, 1, 0), Some(200));

    assert_eq!(ran.cleanup(), "fail", "{:?}", ran.lines.last());
    assert_eq!(ran.cells_cleanup(), ["fail", "fail"]);
}

/// A key left in the system's SIP routing index fails the cleanup gate though
/// no call is active or stored.
#[test]
fn a_key_left_in_the_sip_index_fails_every_cells_cleanup_though_no_call_is_stored() {
    let ran = hosted_batch("indexed", |_| gauges(0, 0, 2), Some(200));

    assert_eq!(ran.cleanup(), "fail", "{:?}", ran.lines.last());
    assert_eq!(ran.cells_cleanup(), ["fail", "fail"]);
}

/// A batch-spec whose end would wait no time, or longer than the ceiling, is
/// refused before any cell runs, naming the field.
#[test]
fn a_reap_wait_of_zero_or_past_the_ceiling_refuses_the_batch_spec() {
    let dir = scratch("reap-bounds");
    for (ms, why) in [(0u64, "above 0"), (600_001, "at most 600000"), (u64::MAX, "at most 600000")]
    {
        let spec = dir.join(format!("batch-spec-{ms}.json"));
        let batch = serde_json::json!({
            "lane": "hosted", "metrics": "127.0.0.1:9", "metrics_reap_ms": ms, "work_dir": dir
        });
        std::fs::write(&spec, batch.to_string()).expect("the batch-spec");
        let ran = Command::new(env!("CARGO_BIN_EXE_pivot-replay"))
            .arg("batch")
            .arg(&spec)
            .stdin(Stdio::null())
            .output()
            .expect("the batch process");
        let stderr = String::from_utf8_lossy(&ran.stderr);
        assert_eq!(ran.status.code(), Some(2), "{ms}: {stderr}");
        assert!(stderr.contains("metrics_reap_ms") && stderr.contains(why), "{ms}: {stderr}");
        assert!(ran.stdout.is_empty(), "{ms}: no protocol line: {:?}", ran.stdout);
    }
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
