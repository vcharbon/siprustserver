//! The deployed process on SIGTERM: it stops serving and exits on its own, at
//! once. As a container's PID 1 a signal with no handler is ignored, so a
//! process that only dies of the default action would serve on until SIGKILL,
//! beside its replacement (two authoritative stores, ADR-0040).

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A loopback port nothing listens on at the time of the call.
fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

#[test]
fn sigterm_closes_the_port_and_exits_cleanly() {
    let addr = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_call-limiter-runner"))
        .env("LIMITER_LISTEN", addr.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let up_by = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < up_by, "the runner never listened on {addr}");
        std::thread::sleep(Duration::from_millis(5));
    }
    let held = TcpStream::connect(addr).unwrap();

    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(killed.success());

    let sent = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if sent.elapsed() > Duration::from_millis(500) {
            let _ = child.kill();
            panic!("the runner still runs 500 ms after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Exit code 0, not death by the signal's default action (which PID 1 never gets).
    assert!(status.success(), "the runner did not handle SIGTERM: {status:?}");
    assert!(TcpStream::connect(addr).is_err(), "the port still accepts after exit");
    drop(held);
}
