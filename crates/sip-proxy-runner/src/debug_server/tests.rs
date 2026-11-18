//! The debug server's start, over real sockets.
use super::*;

/// A taken port leaves the proxy without its debug server, with no error to
/// end its boot; a free one starts it.
#[tokio::test]
async fn a_taken_debug_port_gives_no_server_and_a_free_one_serves() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = holder.local_addr().unwrap();
    assert!(start(taken).await.is_none());
    let server = start("127.0.0.1:0".parse().unwrap()).await.expect("a free port serves");
    assert!(server.addr().ip().is_loopback());
    assert_ne!(server.addr().port(), 0);
}
