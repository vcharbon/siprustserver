//! The brokers the RabbitMQ CDR sink reaches its cluster through, and the
//! order a connect attempt tries them in. One attempt walks every broker from
//! a cursor, each given its share of the connect bound, so a dead or silent
//! node never keeps the sink down while another answers. The cursor starts
//! where the writer's seed points, so workers spread over the nodes; it stays
//! on a node whose session delivered, and moves past one whose attempt or
//! session failed, so a dead node is not tried first again.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

use lapin::uri::AMQPUri;
use tokio::sync::watch;
use tokio::time::Instant;

use super::broker_list::broker_name;
use super::resolver::BrokerResolver;
use super::session::Session;
use super::settings::RabbitMqCdrSettings;
use super::tally::Tally;

/// One broker of the list and the resolution of its host, single-flight.
pub(super) struct Broker {
    pub(super) uri: AMQPUri,
    pub(super) resolver: BrokerResolver,
}

/// Why no broker of the list opened a session: each tried broker's failure,
/// in the order tried.
#[derive(Debug)]
pub(super) struct Unreached(Vec<String>);

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("no broker listed");
        }
        f.write_str(&self.0.join("; "))
    }
}

pub(super) struct Brokers {
    brokers: Vec<Broker>,
    /// The broker the next attempt starts at.
    next: usize,
    /// The broker the live session runs on, until [`Brokers::ended`].
    current: Option<usize>,
}

/// A start drawn per writer from the process's random hasher keys: writers of
/// different processes, and of one process, start on different brokers.
pub(super) fn drawn_start() -> usize {
    let drawn = RandomState::new().hash_one(0u8);
    usize::try_from(drawn % u64::from(u32::MAX)).unwrap_or(0)
}

impl Brokers {
    /// The brokers `uris` lists, the first attempt starting at `first`
    /// (modulo their count); a parked lookup answer older than `max_age` is
    /// not used.
    pub(super) fn new(uris: &[AMQPUri], first: usize, max_age: Duration) -> Self {
        let brokers = uris
            .iter()
            .map(|uri| Broker { uri: uri.clone(), resolver: BrokerResolver::new(max_age) })
            .collect();
        let mut this = Self { brokers, next: 0, current: None };
        this.start_at(first);
        this
    }

    /// The next attempt starts at broker `first` (modulo the count).
    pub(super) fn start_at(&mut self, first: usize) {
        self.next = first.checked_rem(self.brokers.len()).unwrap_or(0);
    }

    /// One connect attempt by `deadline`: each broker from the cursor in
    /// turn, its host resolved and its addresses connected within their
    /// share of the time left, its AMQP handshake within `deadline`, until one
    /// opens a session ([`Session::open`]). A failed or dropped attempt moves
    /// the cursor to the broker after the one it started at; a session opened
    /// sets it to its broker.
    pub(super) async fn open(
        &mut self,
        settings: &RabbitMqCdrSettings,
        deadline: Instant,
        tally: &Tally,
        close_by: &watch::Sender<Option<Instant>>,
    ) -> Result<Session, Unreached> {
        let count = self.brokers.len();
        let start = self.next;
        self.current = None;
        self.start_at(start + 1);
        let mut failures = Vec::new();
        for tried in 0..count {
            if tried > 0 && Instant::now() >= deadline {
                break;
            }
            let index = (start + tried) % count;
            let Some(broker) = self.brokers.get_mut(index) else {
                break;
            };
            let later = count - tried - 1;
            let opened =
                Session::open(settings, broker, later, deadline, tally, close_by.subscribe()).await;
            match opened {
                Ok(session) => {
                    tracing::info!(broker = %broker_name(&broker.uri), "CDR broker connected");
                    self.next = index;
                    self.current = Some(index);
                    return Ok(session);
                }
                Err(e) => failures.push(format!("{}: {e}", broker_name(&broker.uri))),
            }
        }
        if let Some(untried) = count.checked_sub(failures.len()).filter(|n| *n > 0) {
            failures.push(format!("{untried} more not tried by the connect deadline"));
        }
        Err(Unreached(failures))
    }

    /// The live session ended: the next attempt starts at its broker when it
    /// delivered, else at the broker after it.
    pub(super) fn ended(&mut self, delivered: bool) {
        if let Some(index) = self.current.take() {
            self.start_at(if delivered { index } else { index + 1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two writers of one process drawing the same start is a coin toss: 64
    /// of them all on one of two brokers would be a fixed draw.
    #[test]
    fn writers_draw_starts_spread_over_the_brokers() {
        let starts: std::collections::HashSet<usize> = (0..64).map(|_| drawn_start() % 2).collect();
        assert_eq!(starts.len(), 2, "every writer drew the same start");
    }
}
