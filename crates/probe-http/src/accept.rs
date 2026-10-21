//! The accept loop of a [`ProbeServer`](crate::ProbeServer): it takes each
//! connection under the [`MAX_CONNECTIONS`] backstop and serves it on a task
//! of its own. A failed accept never ends it: the probes it answers are what
//! keep the process alive, so a listener lost to one `EMFILE` would get a
//! healthy process restarted.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

use crate::{handle_conn, Served};

/// Light backstop on concurrent connections. The cluster is internal, so this
/// is not a throttle — just a ceiling so a runaway client can't spawn unbounded
/// handler tasks. A handful of probes + one scraper + a flamegraph fit easily.
const MAX_CONNECTIONS: usize = 64;

/// The wait after a failed accept, so a lasting failure (descriptors
/// exhausted) does not spin the loop.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Where the loop takes its connections: a bound [`TcpListener`].
pub(crate) trait Incoming: Send + 'static {
    fn accept(&self) -> impl Future<Output = std::io::Result<TcpStream>> + Send;
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

impl Incoming for TcpListener {
    async fn accept(&self) -> std::io::Result<TcpStream> {
        TcpListener::accept(self).await.map(|(stream, _peer)| stream)
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        TcpListener::local_addr(self)
    }
}

/// Accept on `listener` and serve each connection with `served`, until the
/// task is aborted. A failed accept logs a warning once per run of failures
/// and is retried after [`ACCEPT_PAUSE`].
pub(crate) async fn run(listener: impl Incoming, served: Served, idle_timeout: Duration) {
    #[expect(clippy::disallowed_methods, reason = "MAX_CONNECTIONS is below the permit ceiling")]
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let local = listener.local_addr().ok();
    let mut failing = false;
    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            Err(error) => {
                if !std::mem::replace(&mut failing, true) {
                    tracing::warn!(
                        ?local,
                        %error,
                        "probe accept failed; accepting again after a pause"
                    );
                }
                tokio::time::sleep(ACCEPT_PAUSE).await;
                continue;
            }
        };
        if std::mem::take(&mut failing) {
            tracing::info!(?local, "probe listener accepting again");
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue; // at the backstop cap — drop the connection
        };
        let served = served.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle_conn(stream, served, idle_timeout).await;
        });
    }
}
