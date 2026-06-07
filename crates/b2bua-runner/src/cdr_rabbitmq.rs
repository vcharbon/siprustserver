//! RabbitMQ CDR sink — a [`CdrWriter`] that publishes one JSON-encoded
//! [`CdrRecord`] per terminated call onto an AMQP queue. This is the production
//! adapter that replaces the discarding `NullCdrWriter` when
//! `B2BUA_CDR_RABBITMQ_URL` is set.
//!
//! ## Buffering / "max buffer" guarantee
//! This writer is wired *behind* the existing [`BufferedCdrWriter`] in
//! `main.rs`, so the hot-path `write()` still enqueues non-blocking with
//! drop-on-overload at `B2BUA_CDR_QUEUE` depth — the in-process max buffer is
//! preserved untouched. A single drainer task then calls *this* writer serially,
//! so the channel guarded below is uncontended. On top of that, the broker queue
//! is declared with `x-max-length` + `x-overflow=drop-head`, bounding the
//! *broker-side* buffer too: if the consumer falls behind, RabbitMQ drops the
//! oldest records rather than growing without bound.
//!
//! ## Failure handling
//! CDR delivery is best-effort telemetry, never on the call's critical path. A
//! broker that is down/slow must not stall the drainer: a publish failure drops
//! the connection (so the next record reconnects) and is counted; the missed
//! record is lost, exactly as the in-process buffer drops on overload. The
//! connection is established lazily on first write and re-established after any
//! error.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{build_record, CdrRecord, CdrWriter};
use call::Call;
use lapin::{
    options::{BasicPublishOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use tokio::sync::Mutex;

/// Publishes terminated-call CDRs as JSON onto an AMQP queue.
pub struct RabbitMqCdrWriter {
    url: String,
    queue: String,
    /// Broker-side queue cap (`x-max-length`). `0` disables the bound.
    max_len: i64,
    /// Lazily (re)established connection + channel. Held together so the
    /// connection's IO task stays alive (dropping `Connection` closes it).
    /// `None` until the first successful connect or after a publish error.
    chan: Mutex<Option<(Connection, Channel)>>,
    published: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl RabbitMqCdrWriter {
    /// `url` is an AMQP URI (`amqp://user:pass@host:5672/vhost`); `queue` is the
    /// destination queue name; `max_len` bounds the broker queue (0 = unbounded).
    pub fn new(url: String, queue: String, max_len: i64) -> Self {
        Self {
            url,
            queue,
            max_len,
            chan: Mutex::new(None),
            published: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Connect over the same tokio runtime everything else uses, then declare the
    /// durable, length-bounded destination queue (idempotent — must match the
    /// consumer's declaration argument-for-argument or the broker errors).
    async fn connect(&self) -> Result<(Connection, Channel), lapin::Error> {
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let conn = Connection::connect(&self.url, props).await?;
        let chan = conn.create_channel().await?;
        let mut args = FieldTable::default();
        if self.max_len > 0 {
            // Bound the broker queue; drop the OLDEST record on overflow so a
            // stalled consumer never grows the broker without limit.
            args.insert("x-max-length".into(), AMQPValue::LongLongInt(self.max_len));
            args.insert(
                "x-overflow".into(),
                AMQPValue::LongString(LongString::from("drop-head")),
            );
        }
        chan.queue_declare(
            &self.queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            args,
        )
        .await?;
        Ok((conn, chan))
    }
}

#[async_trait]
impl CdrWriter for RabbitMqCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        let record = build_record(call, terminated_at);
        let payload = match serde_json::to_vec(&record) {
            Ok(p) => p,
            Err(e) => {
                // A record that won't serialize is a bug, not a transient fault;
                // count it as dropped and move on (never poison the drainer).
                self.dropped.fetch_add(1, Ordering::Relaxed);
                eprintln!("cdr-rabbitmq: serialize failed for {}: {e}", record.call_ref);
                return;
            }
        };

        let mut guard = self.chan.lock().await;
        if guard.is_none() {
            match self.connect().await {
                Ok(c) => *guard = Some(c),
                Err(e) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    eprintln!("cdr-rabbitmq: connect to {} failed: {e}", self.url);
                    return;
                }
            }
        }
        let chan = &guard.as_ref().expect("connected above").1;
        // Default exchange, routing key = queue name (direct to the queue).
        // `persistent` so a broker restart keeps queued records (paired with the
        // durable queue declared above).
        match chan
            .basic_publish(
                "",
                &self.queue,
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default().with_delivery_mode(2),
            )
            .await
        {
            Ok(_confirm) => {
                self.published.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                eprintln!("cdr-rabbitmq: publish failed: {e}; will reconnect");
                // Drop the channel/connection so the next write reconnects.
                *guard = None;
            }
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        // Not a test sink — records live in the broker, not in process memory.
        Vec::new()
    }
}
