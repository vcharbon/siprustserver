//! The connection watch of one session: it ends the session as soon as the
//! client reports an error on its connection or channel (a socket that ended
//! or failed, a missed heartbeat, a broker's `connection.close` or
//! `channel.close` with an error code), so a connection that dies while no
//! record is publishing is retired then, its socket shut down and its end
//! logged, rather than at the next record.

use std::sync::Arc;

use b2bua::cdr::BrokerSessionEnd;
use futures_util::StreamExt;
use lapin::{Connection, Event};
use tokio::task::JoinHandle;

use super::session::Health;

/// Spawns the watch of `connection`, ending `health` on its first error event.
/// The task exits then, or when the client's event stream ends; the session
/// aborts it when it drops.
pub(super) fn watch_connection(connection: &Connection, health: Arc<Health>) -> JoinHandle<()> {
    let mut events = connection.events_listener();
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            if let Event::Error(e) = event {
                health.end_on_error(BrokerSessionEnd::ConnectionLost, &e);
                return;
            }
        }
    })
}
