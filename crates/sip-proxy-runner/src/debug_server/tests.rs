//! The debug server's start, over real sockets.
use super::*;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

/// A taken port leaves the proxy without its debug server, with no error to
/// end its boot; a free one serves the debug routes, `/debug/heap` answering
/// 503 since the proxy hands the server no heap dump.
#[tokio::test]
async fn a_taken_debug_port_gives_no_server_and_a_free_one_serves() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = holder.local_addr().unwrap();
    assert!(start(taken).await.is_none());
    let server = start("127.0.0.1:0".parse().unwrap()).await.expect("a free port serves");
    assert!(server.addr().ip().is_loopback());
    let heap = get(server.addr(), "/debug/heap").await;
    assert!(heap.starts_with("HTTP/1.1 503 "), "{heap}");
    let unknown = get(server.addr(), "/debug/none").await;
    assert!(unknown.starts_with("HTTP/1.1 404 "), "{unknown}");
}
