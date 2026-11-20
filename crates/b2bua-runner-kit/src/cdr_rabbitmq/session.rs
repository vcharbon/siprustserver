//! One AMQP connection of the RabbitMQ CDR sink and its channel in publisher
//! confirm mode. A session ends, never to publish again, on the first sign the
//! broker is not delivering: a publish the connection does not take within its
//! bound, a confirm missing past its bound, a channel or connection error, a
//! returned publish, or a connection or channel found closed before a publish;
//! and on a panic of the writer or of its confirm tracker. Ending it shuts its
//! socket down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use amqp_connect::{connect_stream_sharing, open_over, try_by, BrokerStream, Connected};
use lapin::{
    options::{BasicPublishOptions, ConfirmSelectOptions},
    uri::AMQPUri,
    BasicProperties, Channel, Connection,
};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};

use super::brokers::Broker;
use super::confirms::{track_confirms, Pending};
use super::connection_watch::watch_connection;
use super::declaration::declaration;
use super::settings::RabbitMqCdrSettings;
use super::socket_kill::{KillOnDrop, SocketKill};
use super::tally::Tally;

/// The confirm window's permits: `window`, at most the settings' ceiling
/// (far below the semaphore's).
fn window_permits(window: usize) -> Semaphore {
    let bounded = window.min(usize::try_from(super::MAX_WINDOW).unwrap_or(usize::MAX));
    #[expect(clippy::disallowed_methods, reason = "held within MAX_WINDOW")]
    let permits = Semaphore::new(bounded);
    permits
}

/// Why a session could not be opened on one broker.
#[derive(Debug)]
pub(super) enum OpenError {
    /// The broker host did not resolve within its share of the deadline.
    Resolve(std::io::Error),
    /// The TCP connect or the TLS handshake failed or missed the deadline.
    Connect(std::io::Error),
    /// The AMQP handshake, channel, `confirm.select` or declare failed.
    Amqp(lapin::Error),
    /// The AMQP handshake, channel, `confirm.select` or declare was not done
    /// by the deadline.
    AmqpTimedOut,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Resolve(e) => write!(f, "broker host unresolved: {e}"),
            OpenError::Connect(e) => write!(f, "broker connect failed: {e}"),
            OpenError::Amqp(e) => write!(f, "{e}"),
            OpenError::AmqpTimedOut => {
                f.write_str("AMQP handshake not done by the connect deadline")
            }
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
    connection: Connection,
    confirm_timeout: Duration,
    /// Unbounded, yet never holds more than the window: each entry owns a slot.
    /// `None` once the session is closing.
    pending: Option<mpsc::UnboundedSender<Pending>>,
    /// The confirm tracker, awaited by [`Session::close`].
    tracker: Option<JoinHandle<()>>,
    /// The connection watch, aborted when the session drops.
    watch: JoinHandle<()>,
    health: Arc<Health>,
}

impl Session {
    /// Resolves `broker`'s host, connects, opens a channel, selects publisher
    /// confirms and declares the queue, then spawns the confirm tracker
    /// counting into `tally`, bounded by the writer's close deadline
    /// `close_by`, and the connection watch. Everything is done by
    /// `deadline`; the resolution and each TCP connect stop at their share of
    /// it, counted with the `later` brokers the caller tries after this one
    /// ([`amqp_connect::try_by`]). Dropping this future, or a failure
    /// half-way, shuts down whatever socket it connected. The client is
    /// handed the connected stream with auto-recovery off: a connection lost
    /// is this session's end, and the writer opens the next one.
    pub(super) async fn open(
        settings: &RabbitMqCdrSettings,
        broker: &mut Broker,
        later: usize,
        deadline: Instant,
        tally: &Tally,
        close_by: watch::Receiver<Option<Instant>>,
    ) -> Result<Self, OpenError> {
        let uri = &broker.uri;
        let addrs = broker
            .resolver
            .resolve(&uri.authority.host, uri.authority.port, try_by(deadline, later + 1))
            .await
            .map_err(OpenError::Resolve)?;
        let Connected { stream, socket: descriptor } =
            connect_stream_sharing(uri, &settings.tls, &addrs, later, deadline)
                .await
                .map_err(OpenError::Connect)?;
        let socket = Arc::new(SocketKill::default());
        socket.arm(descriptor);
        let guard = KillOnDrop::new(socket.clone());
        let (connection, channel) =
            timeout_at(deadline, confirming_channel(settings, uri.clone(), stream))
                .await
                .map_err(|_| OpenError::AmqpTimedOut)??;
        guard.disarm();

        let health = Arc::new(Health {
            ended_at: OnceLock::new(),
            delivered: AtomicBool::new(false),
            nack_logged: AtomicBool::new(false),
            window: Arc::new(window_permits(settings.bounds.window)),
            socket,
        });
        let (pending, rx) = mpsc::unbounded_channel();
        let tracker = tokio::spawn(track_confirms(rx, health.clone(), close_by, tally.clone()));
        let watch = watch_connection(&connection, health.clone());
        Ok(Self {
            channel,
            connection,
            confirm_timeout: settings.bounds.confirm_timeout,
            pending: Some(pending),
            tracker: Some(tracker),
            watch,
            health,
        })
    }

    /// Publishes no more and waits for the confirm tracker to resolve every
    /// publish of this session, which it does by the writer's close deadline,
    /// then ends the session.
    pub(super) async fn close(mut self) {
        self.pending = None;
        if let Some(tracker) = self.tracker.take() {
            let _ = tracker.await;
        }
    }

    /// Ends the session and hands back its confirm tracker, which resolves
    /// the publishes still outstanding without waiting.
    pub(super) fn into_tracker(mut self) -> Option<JoinHandle<()>> {
        self.tracker.take()
    }

    /// When the session ended, `None` while it lives; an ended session never
    /// publishes again.
    pub(super) fn ended_at(&self) -> Option<Instant> {
        self.health.ended_at.get().copied()
    }

    /// Ends the session for `reason`: it publishes no more, and the writer
    /// replaces it at its next record.
    pub(super) fn end(&self, reason: &str) {
        self.health.end(reason);
    }

    /// Whether the broker acked at least one publish of this session.
    pub(super) fn delivered(&self) -> bool {
        self.health.delivered.load(Ordering::SeqCst)
    }

    /// Ends the session when its client no longer holds the connection and
    /// the channel open: the broker closed one of them, or the connection
    /// failed. The client refuses a publish on either, so a session found so
    /// is ended before a record is handed to it.
    pub(super) fn end_if_disconnected(&self) {
        let connection = self.connection.status();
        let reason = if connection.errored() {
            "the connection failed"
        } else if !connection.connected() {
            "the connection is closed"
        } else if !self.channel.status().connected() {
            "the channel is closed"
        } else {
            return;
        };
        self.health.end(reason);
    }

    /// Publishes `payload`, the record the writer was handed at `handed_at`,
    /// to `queue` (default exchange, `mandatory`, persistent), taking a
    /// window slot and handing the publish to the connection before
    /// `deadline`. `Ok` hands the record to the confirm tracker, which counts
    /// it; `Err` means the caller counts it dropped.
    pub(super) async fn publish(
        &self,
        queue: &str,
        payload: &[u8],
        handed_at: Instant,
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
            "".into(),
            queue.into(),
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
        let pending = Pending {
            confirm,
            handed_at,
            deadline: Instant::now() + self.confirm_timeout,
            _slot: slot,
        };
        let Some(tracker) = &self.pending else {
            return Err(PublishRefused::Ended);
        };
        if tracker.send(pending).is_err() {
            self.health.end("the confirm tracker is gone");
            return Err(PublishRefused::Ended);
        }
        Ok(())
    }
}

/// The AMQP connection over `stream` and its channel in confirm mode, the
/// queue declared on it.
async fn confirming_channel(
    settings: &RabbitMqCdrSettings,
    uri: AMQPUri,
    stream: BrokerStream,
) -> Result<(Connection, Channel), lapin::Error> {
    let connection = open_over(uri, stream).await?;
    let channel = connection.create_channel().await?;
    channel.confirm_select(ConfirmSelectOptions::default()).await?;
    let (options, args) = declaration(settings.declare);
    channel.queue_declare(settings.queue.as_str().into(), options, args).await?;
    Ok((connection, channel))
}

impl Drop for Session {
    fn drop(&mut self) {
        self.watch.abort();
        let _ = self.health.ended_at.set(Instant::now());
        self.health.window.close();
        self.health.socket.kill();
    }
}
