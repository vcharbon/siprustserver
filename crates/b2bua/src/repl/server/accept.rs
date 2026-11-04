//! The accept loop: one serve task per accepted connection, under the cap on
//! concurrent connections (ADR-0045).

use std::time::Duration;

use repl_net::transport::{AcceptError, ReplicationListener};

use super::ReplServer;

/// The pause after a failed accept before the next one.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

impl ReplServer {
    /// Accept connections forever, serving each on a per-connection task. Returns
    /// only when the listener is closed ([`AcceptError::Closed`]). A failed
    /// accept (descriptors exhausted, a handshake aborted) never stops it: a
    /// warning once per run of failures, a pause of 100 ms, and
    /// accepting goes on. A connection accepted while
    /// [`max_connections`](super::ServeConfig::max_connections) are served is
    /// closed and counted, with a warning once per run of such closes. Spawn
    /// this on a task.
    ///
    /// The serve tasks are owned by a [`JoinSet`](tokio::task::JoinSet), NOT
    /// detached. This is load-bearing for crash semantics: when the worker is
    /// aborted, this `run` future is dropped, dropping the `JoinSet` and aborting
    /// every live serve task, cutting their connections so each peer reconnects
    /// to the rebooted node.
    pub async fn run(self, listener: Box<dyn ReplicationListener>) {
        let mut serves = tokio::task::JoinSet::new();
        let mut failing = false;
        let mut capped = false;
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(conn) => {
                        if std::mem::take(&mut failing) {
                            tracing::info!(
                                node = observe::node(),
                                local = %listener.local_addr(),
                                "replication listener accepting again"
                            );
                        }
                        while serves.try_join_next().is_some() {}
                        if serves.len() >= self.config.max_connections {
                            self.count_dropped("cap");
                            if !std::mem::replace(&mut capped, true) {
                                tracing::warn!(
                                    node = observe::node(),
                                    peer = %conn.peer_addr(),
                                    max = self.config.max_connections,
                                    "replication connection closed: the cap on concurrent \
                                     connections is reached"
                                );
                            }
                            continue;
                        }
                        capped = false;
                        let server = self.clone();
                        serves.spawn(observe::keep_node(async move {
                            server.serve_connection(conn).await;
                        }));
                    }
                    Err(AcceptError::Closed) => break,
                    Err(AcceptError::Io(error)) => {
                        if !std::mem::replace(&mut failing, true) {
                            tracing::warn!(
                                node = observe::node(),
                                local = %listener.local_addr(),
                                %error,
                                "replication accept failed; accepting again after a pause"
                            );
                        }
                        tokio::time::sleep(ACCEPT_PAUSE).await;
                    }
                },
                Some(_) = serves.join_next() => {}
            }
        }
    }

    /// Counts one inbound connection closed before it opened a flow.
    pub(super) fn count_dropped(&self, reason: &'static str) {
        if let Some(metrics) = &self.metrics {
            metrics.record_repl_inbound_dropped(reason);
        }
    }
}
