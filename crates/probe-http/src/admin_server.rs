//! [`AdminServer`]: a runner's operator routes and its debug routes on a
//! listener of their own, bound to a loopback address only. An operator
//! reaches them from inside the process's network namespace (`kubectl
//! port-forward`, an SSH tunnel); no other host can. Every path under
//! `/admin/` goes to the runner's [`AdminFn`], the debug routes are
//! `debug.rs`'s, and any other path is `404`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::admin::{AdminFn, AdminReply, PREFIX};
use crate::debug::HeapDumpFn;
use crate::{accept, debug, request, IDLE_TIMEOUT};

/// What an [`AdminServer`] answers besides `GET /debug/flamegraph`, which
/// every one serves.
#[derive(Clone, Default)]
pub struct AdminRoutes {
    /// Every path under `/admin/`; `None` answers `404` there.
    pub operator: Option<AdminFn>,
    /// `GET /debug/heap`; `None` answers `503`.
    pub heap: Option<HeapDumpFn>,
}

/// A running operator server. Aborts its accept loop on drop.
pub struct AdminServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl AdminServer {
    /// Bind on `addr` (port 0 for an ephemeral test port) and serve `routes`.
    /// `Err` of kind `InvalidInput` when `addr` is not a loopback address.
    pub async fn start(addr: SocketAddr, routes: AdminRoutes) -> std::io::Result<Self> {
        loopback_only(addr)?;
        Self::serve_on(TcpListener::bind(addr).await?, routes)
    }

    /// Serve `routes` on a pre-bound listener, whose bind policy is the
    /// caller's. `Err` of kind `InvalidInput` when it is bound to an address
    /// that is not loopback.
    pub fn serve_on(listener: TcpListener, routes: AdminRoutes) -> std::io::Result<Self> {
        let local = listener.local_addr()?;
        loopback_only(local)?;
        let task = tokio::spawn(accept::run(listener, move |stream| {
            let routes = routes.clone();
            async move {
                let _ = serve(stream, routes, IDLE_TIMEOUT).await;
            }
        }));
        Ok(Self { addr: local, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for AdminServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `Err` of kind `InvalidInput` unless `addr` is a loopback address.
fn loopback_only(addr: SocketAddr) -> std::io::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("the operator and debug routes bind a loopback address only, not {addr}"),
    ))
}

/// Answer the one request of `stream`.
async fn serve(
    mut stream: TcpStream,
    routes: AdminRoutes,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let Some(request) = request::read(&mut stream, idle_timeout).await? else {
        return Ok(());
    };
    let route = request.route();
    if debug::answer(&mut stream, route, &request.path, routes.heap.as_ref()).await? {
        return Ok(());
    }
    let reply = routes
        .operator
        .filter(|_| route.starts_with(PREFIX))
        .and_then(|admin| admin(&request.method, route))
        .unwrap_or_else(|| AdminReply::json(404, String::new()));
    stream.write_all(reply.render().as_bytes()).await?;
    stream.flush().await
}
