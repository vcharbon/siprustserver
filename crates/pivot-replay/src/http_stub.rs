//! `replay http-stub <spec.json>`: an environment's long-lived stub HTTP
//! service for the HTTP backends its peers call (`http_net_testkit::stub`), on
//! the real transport. Its spec names each listener and its control service;
//! a caller registers, reads and clears mapping sets through the control.
//!
//! The protocol is lines of JSON on stdout: `{"ready":{"listeners":{name:
//! addr},"control":addr}}` once every listener and the control are bound,
//! naming the bound addresses, and `{"end":{"requests":N}}` once a shutdown
//! was asked for on the control (or a SIGTERM or SIGINT came), every pending
//! answer ended with a close. A listener that does not bind fails the process
//! before its ready line (exit 3); a spec that does not read refuses it (exit
//! 2).

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;

use http_net::{HttpServerHandle, HttpService, HttpTransport, RealHttpNetwork};
use http_net_testkit::stub::{Stub, StubSpec};

use crate::{exit_code, Failure};

/// The process: the spec read, the stub served until a shutdown or a signal,
/// the exit code.
pub fn run(spec_path: &Path) -> i32 {
    let served = read_spec(spec_path).and_then(|spec| {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Failure::Environment(format!("start the runtime: {e}")))?;
        runtime.block_on(serve(spec, signalled(), say))
    });
    match served {
        Ok(_) => 0,
        Err(failure) => {
            eprintln!("replay http-stub: {failure}");
            exit_code(&Err(failure))
        }
    }
}

fn read_spec(spec_path: &Path) -> Result<StubSpec, Failure> {
    let text = std::fs::read_to_string(spec_path)
        .map_err(|e| Failure::Environment(format!("read {}: {e}", spec_path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| Failure::Refused(format!("{} is not a stub spec: {e}", spec_path.display())))
}

/// One protocol line on stdout, flushed.
fn say(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Resolves on SIGTERM or SIGINT; never where neither can be listened for.
async fn signalled() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) =
        (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
    else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// The stub `spec` states served until a shutdown is asked for on its
/// control or `signalled` resolves; each protocol line handed to `say`; how
/// many requests its record keeps at the end.
pub async fn serve(
    spec: StubSpec,
    signalled: impl Future<Output = ()>,
    mut say: impl FnMut(String),
) -> Result<usize, Failure> {
    let stub = Stub::from_spec(&spec);
    let net = RealHttpNetwork::new();
    let mut handles: Vec<Box<dyn HttpServerHandle>> = Vec::new();
    let mut bound: BTreeMap<&str, SocketAddr> = BTreeMap::new();
    let bind = |name: &str, addr: SocketAddr, service: std::sync::Arc<dyn HttpService>| {
        let net = &net;
        let name = name.to_string();
        async move {
            net.serve(addr, service).await.map_err(|e| {
                Failure::Environment(format!("serve the listener {name} at {addr}: {e:?}"))
            })
        }
    };
    for (name, addr) in &spec.listeners {
        let handle = bind(name, *addr, stub.listener(name.as_str())).await?;
        bound.insert(name, handle.local_addr());
        handles.push(handle);
    }
    let control = match spec.control {
        Some(addr) => {
            let handle = bind("control", addr, stub.control()).await?;
            let at = handle.local_addr();
            handles.push(handle);
            Some(at)
        }
        None => None,
    };
    say(serde_json::json!({ "ready": { "listeners": bound, "control": control } }).to_string());
    tokio::select! {
        () = stub.stopped() => {}
        () = signalled => {}
    }
    stub.shutdown();
    drop(handles);
    let requests = stub.records().len();
    say(serde_json::json!({ "end": { "requests": requests } }).to_string());
    Ok(requests)
}
