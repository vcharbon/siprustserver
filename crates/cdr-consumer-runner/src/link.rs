//! The consumer's link to the broker: one connection, its channel, the queue
//! declared and the consume registered, all by one setup deadline. A link
//! holds a duplicate of its socket's descriptor and shuts the socket down when
//! dropped, as does a setup failing half-way or abandoned at its deadline: the
//! client's IO loop runs on a thread of its own and ends only on a socket
//! error or a close handshake, neither of which a silent broker sends.

use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lapin::{
    options::{BasicConsumeOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    uri::AMQPUri,
    Channel, Connection, Consumer,
};
use metric_catalogue::{Family, Labels};
use tokio::time::{timeout_at, Instant};

/// Where the consumer links to and how long a setup may take.
pub struct Target {
    /// The broker's AMQP URI.
    pub url: String,
    /// The queue declared and consumed.
    pub queue: String,
    /// The queue's `x-max-length` (with `drop-head`); 0 or less declares none.
    pub max_len: i64,
    /// The bound of one setup, from resolving the host to the consume registered.
    pub setup_timeout: Duration,
}

/// A link whose consume is registered. Dropping it shuts its socket down.
pub struct Link {
    deliveries: Consumer,
    _channel: Channel,
    _connection: Connection,
    _socket: ShutdownOnDrop,
    _consuming: Consuming,
}

impl Link {
    /// The deliveries of the registered consume; the stream ends when the
    /// broker cancels it, and yields an error when the connection fails.
    pub fn deliveries(&mut self) -> &mut Consumer {
        &mut self.deliveries
    }
}

/// Shuts its socket down, both ways, when dropped.
struct ShutdownOnDrop(TcpStream);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

impl Target {
    /// Opens a link by `setup_timeout`. A setup that fails or misses the
    /// deadline counts one setup failure in `metrics`; a link counts as
    /// consuming while it lives.
    pub async fn open(&self, metrics: &Arc<LinkMetrics>) -> Result<Link, lapin::Error> {
        let deadline = Instant::now() + self.setup_timeout;
        let opened = match timeout_at(deadline, self.setup(deadline)).await {
            Ok(opened) => opened,
            Err(_) => {
                Err(io::Error::new(io::ErrorKind::TimedOut, "the AMQP setup missed its deadline")
                    .into())
            }
        };
        match opened {
            Ok(link) => Ok(Link {
                deliveries: link.deliveries,
                _channel: link.channel,
                _connection: link.connection,
                _socket: link.socket,
                _consuming: Consuming::start(metrics.clone()),
            }),
            Err(e) => {
                metrics.setup_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    async fn setup(&self, deadline: Instant) -> Result<Opened, lapin::Error> {
        let url = self.url.as_str();
        tracing::info!(node = observe::node(), state = "connecting", %url, "AMQP connection");
        let uri: AMQPUri =
            url.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let connected = amqp_connect::connect_uri(&uri, deadline).await?;
        let socket = ShutdownOnDrop(connected.socket);
        let connection = amqp_connect::open_over(uri, connected.stream).await?;
        tracing::info!(node = observe::node(), state = "connected", %url, "AMQP connection");
        let channel = connection.create_channel().await?;

        // Declare the SAME queue the producer declares (durable + bounded), so the
        // two declarations agree argument-for-argument whichever side wins the race.
        let mut args = FieldTable::default();
        if self.max_len > 0 {
            args.insert("x-max-length".into(), AMQPValue::LongLongInt(self.max_len));
            args.insert("x-overflow".into(), AMQPValue::LongString(LongString::from("drop-head")));
        }
        let durable = QueueDeclareOptions { durable: true, ..Default::default() };
        channel.queue_declare(self.queue.as_str().into(), durable, args).await?;

        let deliveries = channel
            .basic_consume(
                self.queue.as_str().into(),
                "cdr-consumer".into(),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await?;
        let queue = self.queue.as_str();
        tracing::info!(node = observe::node(), state = "consuming", %queue, %url, "AMQP connection");
        Ok(Opened { deliveries, channel, connection, socket })
    }
}

/// A setup's parts, before the link counts as consuming.
struct Opened {
    deliveries: Consumer,
    channel: Channel,
    connection: Connection,
    socket: ShutdownOnDrop,
}

/// The link's state on `/metrics`: whether a consume is registered, and how
/// many setups failed, so a broker out of reach shows apart from an idle queue.
#[derive(Default)]
pub struct LinkMetrics {
    consuming: AtomicU64,
    setup_failures: AtomicU64,
}

impl LinkMetrics {
    pub fn prometheus_text(&self) -> String {
        let mut s = String::new();
        CONSUMING.render_value(&mut s, self.consuming.load(Ordering::Relaxed));
        SETUP_FAILURES.render_value(&mut s, self.setup_failures.load(Ordering::Relaxed));
        s
    }
}

/// Counts one consuming link while it lives.
struct Consuming(Arc<LinkMetrics>);

impl Consuming {
    fn start(metrics: Arc<LinkMetrics>) -> Self {
        metrics.consuming.fetch_add(1, Ordering::Relaxed);
        Self(metrics)
    }
}

impl Drop for Consuming {
    fn drop(&mut self) {
        self.0.consuming.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether a consume is registered.
pub const CONSUMING: Family = Family::gauge(
    "cdr_amqp_consuming",
    Labels::None,
    "1 while the consumer's consume is registered on the broker, 0 while it connects or waits to reconnect",
);

/// Setups that failed.
pub const SETUP_FAILURES: Family = Family::counter(
    "cdr_amqp_setup_failures_total",
    Labels::None,
    "AMQP setups (connect, handshake, channel, queue declare, consume) that failed or missed their deadline",
);

/// Every family [`LinkMetrics::prometheus_text`] renders, in order.
pub const FAMILIES: &[Family] = &[CONSUMING, SETUP_FAILURES];
