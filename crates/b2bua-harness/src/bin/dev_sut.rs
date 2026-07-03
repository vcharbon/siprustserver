//! `dev_sut` — a permanently-useful **manual playground SUT** for driving the
//! `loadgen` release binary against a live in-process B2BUA over real loopback
//! UDP.
//!
//! It stands up the SAME System-Under-Test the `loadgen` smoke suite uses
//! (`crates/loadgen/tests/smoke.rs`): a real [`b2bua::B2buaCore`] bound on a
//! `RealSignalingNetwork` (real sockets, wall clock, `TransportKind::Live`) that
//! routes the b-leg to the loadgen's `bob` endpoint and honours the full inbound
//! `X-Api-Call` control surface (destination pin + ADR-0017 `routes` failover
//! plan), so the rerouting shapes exercise the reroute end to end. It then parks
//! until Ctrl-C, keeping the worker tasks alive for as long as you drive load.
//!
//! It is NOT the deployed b2bua — it is the deterministic single-SUT test double,
//! run outside a `#[test]` so a human (or a validation script) can point the real
//! `loadgen` binary at it:
//!
//! ```text
//! # terminal 1 — the SUT (default: :5080, b-leg → 127.0.0.1:6001)
//! cargo run --release -p b2bua-harness --bin dev_sut
//!
//! # terminal 2 — drive the release loadgen against it
//! cargo run --release -p loadgen -- \
//!   --load-profile e2e/loadprofiles/fusion-validation.json \
//!   --target 127.0.0.1:5080 --bind-ip 127.0.0.1 --base-port 6000 \
//!   --correlate to-user --out-dir e2e/runs/load/fusion-validation
//! ```
//!
//! The b-leg destination (`--uas`) must match the loadgen's `bob` endpoint
//! (`--bind-ip` + `--base-port` + 1); the defaults line up with the loadgen's
//! own `--base-port 6000` default.

use std::net::SocketAddr;

use b2bua_harness::B2buaSut;
use layer_harness::TransportKind;
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use std::sync::Arc;
use std::time::Duration;

/// Minimal hand-rolled flag parsing (no clap dep needed for a dev playground):
/// `--sut <addr>`, `--uas <addr>`, `--recv-timeout-ms <n>`, `--relay-header`.
struct DevArgs {
    /// The B2BUA's own listen address (the loadgen's `--target`).
    sut: SocketAddr,
    /// The b-leg destination — the loadgen's `bob` endpoint (base_port + 1). Both
    /// `bob` and the rerouting `bob2` share this one socket on the loadgen side;
    /// the SUT's `X-Api-Call` routes plan pins both to it and the loadgen's
    /// R-URI-user leg picker demuxes them.
    uas: SocketAddr,
    /// The per-recv wall-clock timeout for the SUT's own harness endpoint.
    recv_timeout: Duration,
    /// Relay the loadgen correlation header (`X-Loadgen-Id`) onto every leg — the
    /// production `B2BUA_RELAY_HEADERS`. OFF by default so the SUT models a
    /// third-party B2BUA that relays nothing (the `--correlate to-user`
    /// zero-cooperation path). Turn ON with `--relay-header` to drive the loadgen
    /// with `--correlate header`.
    relay_header: bool,
}

impl DevArgs {
    fn parse() -> Self {
        let mut sut: SocketAddr = "127.0.0.1:5080".parse().unwrap();
        let mut uas: SocketAddr = "127.0.0.1:6001".parse().unwrap();
        let mut recv_timeout = Duration::from_secs(5);
        let mut relay_header = false;
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--sut" => sut = it.next().expect("--sut needs an addr").parse().expect("bad --sut addr"),
                "--uas" => uas = it.next().expect("--uas needs an addr").parse().expect("bad --uas addr"),
                "--recv-timeout-ms" => {
                    let ms: u64 = it.next().expect("--recv-timeout-ms needs a value").parse().expect("bad --recv-timeout-ms");
                    recv_timeout = Duration::from_millis(ms);
                }
                "--relay-header" => relay_header = true,
                "-h" | "--help" => {
                    eprintln!(
                        "dev_sut — in-process B2BUA playground for driving the loadgen binary.\n\
                         \n\
                         USAGE: dev_sut [--sut <addr>] [--uas <addr>] [--recv-timeout-ms <n>] [--relay-header]\n\
                         \n\
                           --sut <addr>            B2BUA listen address (loadgen --target). Default 127.0.0.1:5080\n\
                           --uas <addr>            b-leg destination = loadgen bob endpoint (base_port+1). Default 127.0.0.1:6001\n\
                           --recv-timeout-ms <n>   SUT harness per-recv wall-clock timeout. Default 5000\n\
                           --relay-header          relay X-Loadgen-Id onto every leg (drive with --correlate header).\n\
                                                   Default OFF (third-party-SUT shape; drive with --correlate to-user).\n"
                    );
                    std::process::exit(0);
                }
                other => panic!("unknown flag {other:?} (try --help)"),
            }
        }
        DevArgs { sut, uas, recv_timeout, relay_header }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = DevArgs::parse();

    // The SAME wiring as `loadgen::tests::smoke::setup_shaped`, minus the mux
    // (the loadgen binary owns alice/bob/charlie): a real-socket harness under
    // the wall clock, and a b2bua that honours the full X-Api-Call surface so the
    // rerouting shapes' `routes` failover plan is walked.
    let net: Arc<dyn SignalingNetwork> = Arc::new(RealSignalingNetwork::new());
    let h = Harness::with_network_and_clock(
        "dev-sut",
        net,
        Clock::system(),
        TransportKind::Live,
        args.recv_timeout,
    );
    // Infra harness; the loadgen runs its own per-call RFC audit over the mux.
    h.disarm_cseq_gate();

    let (uas_host, uas_port) = (args.uas.ip().to_string(), args.uas.port());
    let relay_header = args.relay_header;
    let b2bua = B2buaSut::route_api_call(&uas_host, uas_port)
        .tune(move |c| {
            if relay_header {
                c.relay_headers = vec!["X-Loadgen-Id".to_string()];
            }
        })
        .start(&h, "b2bua", &args.sut.to_string())
        .await;

    eprintln!(
        "[dev_sut] B2BUA (X-Api-Call aware) on {} — b-leg → {} — recv_timeout={:?} — relay_header={}",
        b2bua.addr, args.uas, args.recv_timeout, args.relay_header
    );
    eprintln!(
        "[dev_sut] drive it: cargo run --release -p loadgen -- --target {} --bind-ip {} --base-port {} --correlate {}",
        b2bua.addr,
        args.uas.ip(),
        args.uas.port().saturating_sub(1),
        if args.relay_header { "header" } else { "to-user" },
    );
    eprintln!("[dev_sut] Ctrl-C to stop.");

    // Park until Ctrl-C, keeping the harness + SUT worker tasks alive. (`_h`/
    // `b2bua` are held by this scope for the whole run; dropping them tears the
    // SUT down.)
    let _h = h;
    match tokio::signal::ctrl_c().await {
        Ok(()) => eprintln!("\n[dev_sut] Ctrl-C — shutting the SUT down."),
        Err(e) => eprintln!("[dev_sut] failed to listen for Ctrl-C: {e}; exiting."),
    }
    drop(b2bua);
}
