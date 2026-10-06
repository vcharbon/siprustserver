//! One AMQP connection of the RabbitMQ CDR sink and its channel in publisher
//! confirm mode. A session ends, never to publish again, on the first sign the
//! broker is not delivering: a publish the connection does not take within its
//! bound, a confirm missing past its bound, a channel or connection error, or a
//! returned publish. Ending it shuts its socket down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use b2bua::metrics::B2buaMetrics;
use lapin::{
    options::{BasicPublishOptions, ConfirmSelectOptions},
    uri::AMQPUri,
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{timeout_at, Instant};

use super::confirms::{track_confirms, Pending};
use super::connector::connect_socket;
use super::declaration::declaration;
use super::settings::RabbitMqCdrSettings;
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

/// What the session and its confirm tracker share.
pub(super) struct Health {
    /// When the session ended; set once.
    ended_at: OnceLock<Instant>,
    /// Whether the broker acked at least one publish of this session.
    pub(super) delivered: AtomicBool,
    /// Whether a nack was already logged for this session.
    pub(super) nack_logged: AtomicBool,
    window: Arc<Semaphore>,
    socket: Arc<SocketKill>,
}

impl Health {
    /// Ends the session: no further publish, the window closed (a publish
    /// waiting for a slot returns), the socket shut down. Logs the first reason.
    pub(super) fn end(&self, reason: &str) {
        if self.ended_at.set(Instant::now()).is_ok() {
            tracing::warn!(reason, "CDR broker connection dropped; the next record reconnects");
        }
        self.window.close();
        self.socket.kill();
    }

    pub(super) fn ended(&self) -> bool {
        self.ended_at.get().is_some()
    }
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
            let deadline = std::time::Instant::now() + settings.bounds.connect_timeout;
            Box::new(move |uri: &AMQPUri| connect_socket(uri, deadline, &socket))
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
        if self.health.ended() {
            // The slot came free because the session ended.
            return Err(PublishRefused::Ended);
        }
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
