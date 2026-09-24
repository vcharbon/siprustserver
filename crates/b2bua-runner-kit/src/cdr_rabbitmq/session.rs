//! One AMQP connection of the RabbitMQ CDR sink, its channel in publisher
//! confirm mode, and the tracker that turns each publish's confirm into
//! exactly one `cdr_written_total` or `cdr_dropped_total`.
//!
//! A session ends (never to publish again) on the first sign the broker is
//! not delivering: a publish the connection does not take within its bound, a
//! confirm that does not arrive within its bound, a channel or connection
//! error, or a returned (unroutable) publish. Ending it shuts its socket down
//! and fails the records it still holds unconfirmed.

use std::io;
use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use b2bua::metrics::B2buaMetrics;
use lapin::{
    options::{BasicPublishOptions, ConfirmSelectOptions, QueueDeclareOptions},
    publisher_confirm::{Confirmation, PublisherConfirm},
    tcp::{HandshakeResult, TLSConfig, TcpStream},
    types::{AMQPValue, FieldTable, LongString},
    uri::{AMQPScheme, AMQPUri},
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, timeout_at, Instant};

use super::settings::{CdrQueueDeclare, RabbitMqCdrSettings};
use super::socket_kill::{KillOnDrop, SocketKill};

/// Why a session could not be opened.
#[derive(Debug)]
pub(super) enum OpenError {
    /// The URL is not an AMQP URI.
    Uri(String),
    /// Connect, handshake, channel, `confirm.select` or declare failed.
    Amqp(lapin::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Uri(e) => write!(f, "not an AMQP URI: {e}"),
            OpenError::Amqp(e) => write!(f, "{e}"),
        }
    }
}

impl From<lapin::Error> for OpenError {
    fn from(e: lapin::Error) -> Self {
        OpenError::Amqp(e)
    }
}

/// Why a record was not handed to the broker; it counts one dropped record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishRefused {
    /// Every window slot stayed taken past the publish bound; the session lives.
    WindowFull,
    /// The session ended, before or during this publish.
    Ended,
}

/// The queue declaration [`CdrQueueDeclare`] states.
pub(super) fn declaration(declare: CdrQueueDeclare) -> (QueueDeclareOptions, FieldTable) {
    let mut args = FieldTable::default();
    match declare {
        CdrQueueDeclare::Own { max_len } => {
            if max_len > 0 {
                // Drop the OLDEST record on overflow so a stalled consumer
                // never grows the broker without limit.
                args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
                args.insert(
                    "x-overflow".into(),
                    AMQPValue::LongString(LongString::from("drop-head")),
                );
            }
            (QueueDeclareOptions { durable: true, ..Default::default() }, args)
        }
        CdrQueueDeclare::Existing => {
            (QueueDeclareOptions { passive: true, ..Default::default() }, args)
        }
    }
}

/// What the session and its confirm tracker share.
struct Health {
    /// When the session ended; set once.
    ended_at: OnceLock<Instant>,
    /// Whether the broker acked at least one publish of this session.
    delivered: AtomicBool,
    /// Whether a nack was already logged for this session.
    nack_logged: AtomicBool,
    window: Arc<Semaphore>,
    socket: Arc<SocketKill>,
}

impl Health {
    /// Ends the session: no further publish, the window closed (a publish
    /// waiting for a slot returns), the socket shut down. Logs the first reason.
    fn end(&self, reason: &str) {
        if self.ended_at.set(Instant::now()).is_ok() {
            tracing::warn!(reason, "CDR broker connection dropped; the next record reconnects");
        }
        self.window.close();
        self.socket.kill();
    }

    fn ended(&self) -> bool {
        self.ended_at.get().is_some()
    }
}

/// A publish awaiting its confirm, holding one window slot until resolved.
struct Pending {
    confirm: PublisherConfirm,
    deadline: Instant,
    _slot: OwnedSemaphorePermit,
}

/// One connection + confirm-mode channel. Dropping it ends it.
pub(super) struct Session {
    channel: Channel,
    _connection: Connection,
    confirm_timeout: Duration,
    /// Unbounded, yet never holds more than the window: each entry owns a slot.
    pending: mpsc::UnboundedSender<Pending>,
    health: Arc<Health>,
}

impl Session {
    /// Connects, opens a channel, selects publisher confirms and declares the
    /// queue, then spawns the confirm tracker recording into `metrics`. Not
    /// bounded here: the caller bounds it, and dropping this future (or a
    /// failure half-way) shuts down whatever socket it connected.
    // `HandshakeResult` is the AMQP client's connector contract.
    #[allow(clippy::result_large_err)]
    pub(super) async fn open(
        settings: &RabbitMqCdrSettings,
        metrics: &B2buaMetrics,
    ) -> Result<Self, OpenError> {
        let uri: AMQPUri = settings.url.parse().map_err(OpenError::Uri)?;
        let socket = Arc::new(SocketKill::default());
        let guard = KillOnDrop::new(socket.clone());
        let connect = {
            let socket = socket.clone();
            let tcp_timeout = settings.bounds.connect_timeout;
            Box::new(move |uri: &AMQPUri| connect_socket(uri, tcp_timeout, &socket))
        };
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let connection = Connection::connector(uri, connect, props).await?;
        let channel = connection.create_channel().await?;
        channel.confirm_select(ConfirmSelectOptions::default()).await?;
        let (options, args) = declaration(settings.declare);
        channel.queue_declare(&settings.queue, options, args).await?;
        guard.disarm();

        let health = Arc::new(Health {
            ended_at: OnceLock::new(),
            delivered: AtomicBool::new(false),
            nack_logged: AtomicBool::new(false),
            window: Arc::new(Semaphore::new(settings.bounds.window)),
            socket,
        });
        let (pending, rx) = mpsc::unbounded_channel();
        tokio::spawn(track_confirms(rx, health.clone(), metrics.clone()));
        Ok(Self {
            channel,
            _connection: connection,
            confirm_timeout: settings.bounds.confirm_timeout,
            pending,
            health,
        })
    }

    /// When the session ended, `None` while it lives; an ended session never
    /// publishes again.
    pub(super) fn ended_at(&self) -> Option<Instant> {
        self.health.ended_at.get().copied()
    }

    /// Whether the broker acked at least one publish of this session.
    pub(super) fn delivered(&self) -> bool {
        self.health.delivered.load(Ordering::SeqCst)
    }

    /// Publishes `payload` to `queue` (default exchange, `mandatory`,
    /// persistent), taking a window slot and handing the publish to the
    /// connection before `deadline`. `Ok` hands the record to the confirm
    /// tracker, which counts it; `Err` means the caller counts it dropped.
    pub(super) async fn publish(
        &self,
        queue: &str,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<(), PublishRefused> {
        if self.health.ended() {
            return Err(PublishRefused::Ended);
        }
        let slot = match timeout_at(deadline, self.health.window.clone().acquire_owned()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(_closed)) => return Err(PublishRefused::Ended),
            Err(_) => return Err(PublishRefused::WindowFull),
        };
        let publish = self.channel.basic_publish(
            "",
            queue,
            BasicPublishOptions { mandatory: true, immediate: false },
            payload,
            BasicProperties::default().with_delivery_mode(2),
        );
        let confirm = match timeout_at(deadline, publish).await {
            Ok(Ok(confirm)) => confirm,
            Ok(Err(e)) => {
                self.health.end(&format!("publish failed: {e}"));
                return Err(PublishRefused::Ended);
            }
            Err(_) => {
                self.health.end("the connection took no publish within the publish bound");
                return Err(PublishRefused::Ended);
            }
        };
        let pending =
            Pending { confirm, deadline: Instant::now() + self.confirm_timeout, _slot: slot };
        if self.pending.send(pending).is_err() {
            self.health.end("the confirm tracker is gone");
            return Err(PublishRefused::Ended);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.health.ended_at.set(Instant::now());
        self.health.window.close();
        self.health.socket.kill();
    }
}

/// How one confirm resolved.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Acked,
    Nacked,
    Returned,
    /// The channel or connection failed with the confirm outstanding.
    Lost(String),
    /// No confirm within the confirm bound.
    TimedOut,
    /// The session ended before the confirm arrived.
    SessionEnded,
}

/// Resolves every tracked publish, in publish order, to exactly one written
/// or dropped record. A publish still unconfirmed when the session ends is
/// dropped without waiting; one already confirmed keeps its confirm. Exits
/// once the session is gone and every tracked publish is resolved.
async fn track_confirms(
    mut rx: mpsc::UnboundedReceiver<Pending>,
    health: Arc<Health>,
    metrics: B2buaMetrics,
) {
    while let Some(Pending { confirm, deadline, _slot }) = rx.recv().await {
        let outcome = if health.ended() {
            // Only a confirm already received counts; nothing is awaited.
            match timeout(Duration::ZERO, confirm).await {
                Ok(result) => outcome_of(result),
                Err(_) => Outcome::SessionEnded,
            }
        } else {
            match timeout_at(deadline, confirm).await {
                Ok(result) => outcome_of(result),
                Err(_) => Outcome::TimedOut,
            }
        };
        match outcome {
            Outcome::Acked => {
                health.delivered.store(true, Ordering::SeqCst);
                metrics.bump_cdr_written();
            }
            Outcome::Nacked => {
                metrics.bump_cdr_dropped();
                if !health.nack_logged.swap(true, Ordering::SeqCst) {
                    tracing::warn!("CDR publish nacked by the broker; nacked records are dropped");
                }
            }
            Outcome::Returned => {
                metrics.bump_cdr_dropped();
                health.end("publish returned unroutable: the queue is gone");
            }
            Outcome::Lost(e) => {
                metrics.bump_cdr_dropped();
                health.end(&format!("confirm lost: {e}"));
            }
            Outcome::TimedOut => {
                metrics.bump_cdr_dropped();
                health.end("no confirm within the confirm bound");
            }
            Outcome::SessionEnded => metrics.bump_cdr_dropped(),
        }
    }
}

fn outcome_of(result: lapin::Result<Confirmation>) -> Outcome {
    match result {
        Ok(Confirmation::Ack(None)) => Outcome::Acked,
        Ok(Confirmation::Ack(Some(_returned))) => Outcome::Returned,
        Ok(Confirmation::Nack(_)) => Outcome::Nacked,
        // Only a channel without confirms answers this; count it lost.
        Ok(Confirmation::NotRequested) => Outcome::Lost("confirms not selected".into()),
        Err(e) => Outcome::Lost(e.to_string()),
    }
}

/// Connects the socket of `uri` (each resolved address in turn, `tcp_timeout`
/// each), arms `kill` with it, then wraps it as the AMQP client expects: TLS
/// for `amqps`, non-blocking.
#[allow(clippy::result_large_err)] // the AMQP client's connector contract
fn connect_socket(uri: &AMQPUri, tcp_timeout: Duration, kill: &SocketKill) -> HandshakeResult {
    let host = uri.authority.host.as_str();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} resolves to nothing"));
    let mut connected = None;
    for addr in (host, uri.authority.port).to_socket_addrs()? {
        match StdTcpStream::connect_timeout(&addr, tcp_timeout) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let socket = connected.ok_or(last)?;
    socket.set_nodelay(true)?;
    kill.arm(socket.try_clone()?);
    let stream = TcpStream::from_std(socket)?;
    let stream = match uri.scheme {
        AMQPScheme::AMQP => stream,
        AMQPScheme::AMQPS => stream.into_tls(host, TLSConfig::default())?,
    };
    stream.set_nonblocking(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owned_queue_is_declared_durable_and_bounded_drop_head() {
        let (opts, args) = declaration(CdrQueueDeclare::Own { max_len: 100_000 });
        assert!(opts.durable && !opts.passive);
        let inner = args.inner();
        assert_eq!(inner.get("x-max-length"), Some(&AMQPValue::LongLongInt(100_000)));
        assert_eq!(
            inner.get("x-overflow"),
            Some(&AMQPValue::LongString(LongString::from("drop-head")))
        );
    }

    #[test]
    fn an_owned_unbounded_queue_is_declared_durable_without_arguments() {
        let (opts, args) = declaration(CdrQueueDeclare::Own { max_len: 0 });
        assert!(opts.durable && !opts.passive);
        assert!(args.inner().is_empty());
    }

    #[test]
    fn a_broker_held_queue_is_declared_passively_without_arguments() {
        let (opts, args) = declaration(CdrQueueDeclare::Existing);
        assert!(opts.passive, "an existing queue is never (re)declared");
        assert!(args.inner().is_empty(), "no argument of the broker's queue is restated");
    }

    #[test]
    fn only_a_plain_ack_is_a_written_record() {
        assert_eq!(outcome_of(Ok(Confirmation::Ack(None))), Outcome::Acked);
        assert_eq!(outcome_of(Ok(Confirmation::Nack(None))), Outcome::Nacked);
        assert!(matches!(outcome_of(Ok(Confirmation::NotRequested)), Outcome::Lost(_)));
        assert!(matches!(
            outcome_of(Err(lapin::Error::InvalidChannelState(lapin::ChannelState::Closed))),
            Outcome::Lost(_)
        ));
    }
}
