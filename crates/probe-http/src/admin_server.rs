//! [`AdminServer`]: a runner's operator routes on a listener of their own,
//! bound to a loopback address only. An operator reaches them from inside the
//! process's network namespace (`kubectl exec`, `kubectl port-forward`); no
//! other host can. Every path under `/admin/` goes to the runner's
//! [`AdminFn`]; any other path is `404`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::admin::{AdminFn, AdminReply, PREFIX};
use crate::{accept, request, IDLE_TIMEOUT};

/// A running operator server. Aborts its accept loop on drop.
pub struct AdminServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl AdminServer {
    /// Bind on `addr` (port 0 for an ephemeral test port) and serve `admin`.
    /// `Err` of kind `InvalidInput` when `addr` is not a loopback address.
    pub async fn start(addr: SocketAddr, admin: AdminFn) -> std::io::Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("the operator routes bind a loopback address only, not {addr}"),
            ));
        }
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let task = tokio::spawn(accept::run(listener, move |stream| {
            let admin = admin.clone();
            async move {
                let _ = serve(stream, admin, IDLE_TIMEOUT).await;
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

/// Answer the one request of `stream`.
async fn serve(
    mut stream: TcpStream,
    admin: AdminFn,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let Some(request) = request::read(&mut stream, idle_timeout).await? else {
        return Ok(());
    };
    let route = request.route();
    let reply = route
        .starts_with(PREFIX)
        .then(|| admin(&request.method, route))
        .flatten()
        .unwrap_or_else(|| AdminReply::json(404, String::new()));
    stream.write_all(reply.render().as_bytes()).await?;
    stream.flush().await
}
