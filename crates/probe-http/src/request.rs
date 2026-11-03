//! The request line of one connection, read under the fully-idle cut: a
//! connection that sends nothing within the window is dropped.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// The method and path of a request; the path keeps its query.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
}

impl Request {
    /// The path without its query.
    pub(crate) fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

/// The request `stream` opens with; `None` when it sent nothing within
/// `idle_timeout`. A missing method reads `GET`, a missing path `/`.
pub(crate) async fn read(
    stream: &mut TcpStream,
    idle_timeout: Duration,
) -> std::io::Result<Option<Request>> {
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(idle_timeout, stream.read(&mut buf)).await {
        Ok(read) => read?,
        Err(_elapsed) => return Ok(None),
    };
    let req = String::from_utf8_lossy(buf.get(..n).unwrap_or(&buf));
    let mut line = req.split_whitespace();
    let method = line.next().unwrap_or("GET").to_string();
    let path = line.next().unwrap_or("/").to_string();
    Ok(Some(Request { method, path }))
}
