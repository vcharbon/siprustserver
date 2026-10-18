//! `RealHttpNetwork` — the production transport (feature `real`).
//!
//! Server: a tokio `TcpListener` (`listener`: explicit backlog, paced accept
//! errors) handing each connection, `TCP_NODELAY` set, to hyper's HTTP/1.1
//! `serve_connection` (keep-alive on by default, so a client's pooled
//! connection serves many requests). Each request is answered with its remote
//! address (`peer`) and its receipt (`receipt`) in scope. Client: a pooled `reqwest::Client` built
//! from [`ClientSettings`] (connect timeout, idle timeout, TCP keepalive, TCP
//! user timeout, a ceiling on every request), shared by the addressed
//! [`request`](HttpTransport::request); a backend gets a client of its own
//! ([`HttpTransport::client`], `real_client`).
//!
//! A body is carried up to [`MAX_BODY_BYTES`] either way.
//!
//! No TLS: this is internal cluster `http://` traffic. The caller owns its
//! budget; this layer surfaces transport failures as [`HttpError`],
//! classified as `cause` reads them.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body, Incoming};
use hyper::header::{HeaderValue, CONNECTION};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::task::{JoinHandle, JoinSet};

use super::receipt::{self, Receipt};
use super::{
    listener, peer, BindError, HttpAnswer, HttpClient, HttpError, HttpRequest, HttpResponse,
    HttpServerHandle, HttpService, HttpTransport, MAX_BODY_BYTES,
};
use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;

/// The real HTTP transport. Clone shares the pooled client.
#[derive(Clone)]
pub struct RealHttpNetwork {
    client: reqwest::Client,
}

impl Default for RealHttpNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl RealHttpNetwork {
    /// Build with a pooled keep-alive client under the default
    /// [`ClientSettings`] (no TLS).
    pub fn new() -> Self {
        Self::with_settings(&ClientSettings::default())
    }

    /// Build with a pooled keep-alive client under `settings` (no TLS).
    pub fn with_settings(settings: &ClientSettings) -> Self {
        Self { client: super::real_client::build(settings, None) }
    }
}

/// A running real server: aborts its accept loop, and with it every open
/// connection, on drop.
struct RealServerHandle {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl HttpServerHandle for RealServerHandle {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RealServerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl HttpTransport for RealHttpNetwork {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        let listener = listener::bind(addr).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                BindError::AlreadyInUse(addr)
            } else {
                BindError::Io(e.to_string())
            }
        })?;
        let local = listener.local_addr().map_err(|e| BindError::Io(e.to_string()))?;

        let task = tokio::spawn(async move {
            // Dropped with this task when the handle aborts it, which aborts
            // every connection still open (a withheld answer included).
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            // Answers are small: none waits on Nagle for the client's ACK.
                            let _ = stream.set_nodelay(true);
                            connections.spawn(serve_connection(stream, peer, service.clone()));
                        }
                        Err(e) => {
                            if let Some(backoff) = listener::accept_backoff(&e) {
                                tracing::debug!(error = %e, ?backoff, "http server accept failed");
                                tokio::time::sleep(backoff).await;
                            }
                        }
                    },
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });

        Ok(Box::new(RealServerHandle { addr: local, task }))
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        // `req.path` is a path-and-query target, so the query string rides along.
        let url = format!("http://{dst}{}", req.path);
        super::real_client::send(&self.client, &dst.to_string(), &url, req).await
    }

    fn client(
        self: Arc<Self>,
        target: Arc<ResolvedTarget>,
        settings: &ClientSettings,
    ) -> Arc<dyn HttpClient> {
        Arc::new(super::real_client::NamedClient::new(target, settings))
    }
}

/// Serve one accepted connection until the peer closes it. A service
/// answering [`HttpAnswer::Abort`] fails the exchange, and hyper closes the
/// connection without writing a response (RFC 9112 §9.6).
async fn serve_connection(stream: TcpStream, peer: SocketAddr, service: Arc<dyn HttpService>) {
    let io = TokioIo::new(stream);
    let handler = service_fn(move |req: Request<Incoming>| {
        // hyper calls this once the head is read: the receipt precedes the body.
        let received = Receipt::now();
        let svc = service.clone();
        async move {
            let method = req.method().to_string();
            // path_and_query preserves the query string (`?debug=true`); fall
            // back to the bare path.
            let path = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| req.uri().path().to_string());
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let body = req.into_body();
            if body.size_hint().lower() > MAX_BODY_BYTES as u64 {
                return Ok(content_too_large());
            }
            let body = match Limited::new(body, MAX_BODY_BYTES).collect().await {
                Ok(collected) => collected.to_bytes().to_vec(),
                Err(e) if e.is::<LengthLimitError>() => return Ok(content_too_large()),
                Err(e) => return Err(std::io::Error::other(e)),
            };
            let req = HttpRequest { method, path, headers, body };
            let answer = receipt::scope(received, peer::scope(peer, svc.answer(req)));
            let resp = match answer.await {
                HttpAnswer::Response(resp) => resp,
                HttpAnswer::Abort => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "the service closed the connection without a response",
                    ));
                }
            };
            let mut builder = Response::builder().status(resp.status);
            for (name, value) in &resp.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let built = builder
                .body(Full::new(Bytes::from(resp.body)))
                .expect("status + headers + full body always builds a response");
            Ok(built)
        }
    });
    let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, handler).await;
}

/// `413 Content Too Large` (RFC 9110 §15.5.14) closing the connection: the
/// rest of the request body is never read. A declared length past the cap is
/// answered on the head, before any of the body.
fn content_too_large() -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::new()));
    *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    resp.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}
