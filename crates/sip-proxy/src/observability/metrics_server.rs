//! [`MetricsServer`] — the Prometheus `/metrics` HTTP endpoint (port of
//! `observability/MetricsServer.ts`). A tiny hand-rolled tokio TCP handler (no
//! HTTP framework dep): it answers `GET /metrics` with the
//! [`ProxyMetrics::prometheus_text`] body and `GET /healthz` with `ok`;
//! everything else is `404`.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::metrics::ProxyMetrics;

/// A running metrics HTTP server. Aborts its accept loop on drop.
pub struct MetricsServer {
    addr: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl MetricsServer {
    /// Bind on `addr` (use port 0 for an ephemeral test port) and spawn the
    /// accept loop. The bound address is available via [`MetricsServer::addr`].
    pub async fn start(addr: std::net::SocketAddr, metrics: Arc<ProxyMetrics>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let m = metrics.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(stream, m).await;
                });
            }
        });
        Ok(Self { addr: local, task })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_conn(mut stream: TcpStream, metrics: Arc<ProxyMetrics>) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");

    let (status, content_type, body) = match path {
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", metrics.prometheus_text()),
        "/healthz" => ("200 OK", "text/plain", "ok\n".to_string()),
        _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::metrics::{Direction, MessageResult};

    #[tokio::test]
    async fn serves_prometheus_exposition() {
        let metrics = Arc::new(ProxyMetrics::new());
        metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        let server = MetricsServer::start("127.0.0.1:0".parse().unwrap(), metrics).await.unwrap();

        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"));
        assert!(text.contains("# TYPE sip_messages_total counter"));
        assert!(text.contains("sip_routing_duration_seconds_count"));
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let server = MetricsServer::start("127.0.0.1:0".parse().unwrap(), Arc::new(ProxyMetrics::new())).await.unwrap();
        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        stream.write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        assert!(String::from_utf8_lossy(&resp).contains("404 Not Found"));
    }
}
