//! `RealHttpNetwork` — the production transport (feature `real`).
//!
//! Server: a tokio `TcpListener` accept loop handing each connection to hyper's
//! HTTP/1.1 `serve_connection` (keep-alive on by default, so a client's pooled
//! connection serves many requests). Client: a shared, pooled `reqwest::Client`
//! — the b2bua makes many small calls per second, so connection reuse matters.
//!
//! No TLS: this is internal cluster `http://` traffic. The fail-open timeout
//! still lives in the caller; this layer surfaces transport failures as
//! [`HttpError`].

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{
    BindError, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport,
};

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
    /// Build with a pooled keep-alive client (no TLS).
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest client builds with default (no-tls) config");
        Self { client }
    }
}

/// A running real server: aborts its accept loop on drop.
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
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                BindError::AlreadyInUse(addr)
            } else {
                BindError::Io(e.to_string())
            }
        })?;
        let local = listener.local_addr().map_err(|e| BindError::Io(e.to_string()))?;

        let task = tokio::spawn(async move {
            loop {
                let (stream, _peer) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let svc = service.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let handler = service_fn(move |req: Request<Incoming>| {
                        let svc = svc.clone();
                        async move {
                            let method = req.method().to_string();
                            // path_and_query preserves the query string
                            // (`?debug=true`); fall back to the bare path.
                            let path = req
                                .uri()
                                .path_and_query()
                                .map(|pq| pq.as_str().to_string())
                                .unwrap_or_else(|| req.uri().path().to_string());
                            let headers = req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                                })
                                .collect();
                            let body = req
                                .into_body()
                                .collect()
                                .await
                                .map(|c| c.to_bytes().to_vec())
                                .unwrap_or_default();
                            let resp = svc
                                .handle(HttpRequest {
                                    method,
                                    path,
                                    headers,
                                    body,
                                })
                                .await;
                            let mut builder = Response::builder().status(resp.status);
                            for (name, value) in &resp.headers {
                                builder = builder.header(name.as_str(), value.as_str());
                            }
                            let built = builder
                                .body(Full::new(Bytes::from(resp.body)))
                                .expect("status + headers + full body always builds a response");
                            Ok::<_, std::convert::Infallible>(built)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, handler)
                        .await;
                });
            }
        });

        Ok(Box::new(RealServerHandle { addr: local, task }))
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        // `req.path` is a path-and-query target, so the query string rides along.
        let url = format!("http://{dst}{}", req.path);
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| HttpError::Io {
            addr: dst,
            reason: format!("bad method: {e}"),
        })?;
        let mut builder = self.client.request(method, &url);
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let resp = builder.body(req.body).send().await.map_err(|e| {
            if e.is_connect() {
                HttpError::Connect(dst)
            } else {
                HttpError::Io {
                    addr: dst,
                    reason: e.to_string(),
                }
            }
        })?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| HttpError::Io {
                addr: dst,
                reason: e.to_string(),
            })?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}
