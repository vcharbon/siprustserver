//! The RabbitMQ CDR sink a runner composes from env ([`RabbitMqCdrSettings`]):
//! [`RabbitMqCdrWriter`] publishes one record per terminated call, encoded by
//! its [`CdrEncoder`] (default [`JsonRecordEncoder`]).
//!
//! Contract: each record handed to `write` counts once, in `cdr_written_total`
//! when the broker acked it on a confirm-mode channel, else in
//! `cdr_dropped_total`. Dropped means unconfirmed, not lost: a record whose ack
//! never arrived may still reach the queue. The writer's
//! [`CdrDeliveryObserver`] is told each of those ends, with the time since
//! `write` was handed the record. `close` publishes nothing more and
//! returns once every publish of every session is counted, by its deadline at
//! the latest; a write still in flight then counts its own record.
//!
//! Bounds: the writer runs behind the kit's `BufferedCdrWriter` drainer (the
//! grammar refuses `B2BUA_CDR_QUEUE=0` beside a URL); every broker wait is
//! bounded by [`CdrDeliveryBounds`], so a broker fault becomes dropped records,
//! never a stalled call path or growing memory.

mod backoff;
mod broker_list;
mod brokers;
mod confirms;
mod connection_watch;
mod declaration;
mod resolver;
mod session;
mod settings;
mod socket_kill;
mod tally;

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{CdrDeliveryObserver, CdrEncoder, CdrRecord, CdrWriter, JsonRecordEncoder};
use b2bua::metrics::B2buaMetrics;
use call::Call;
use sip_clock::wall::millis_u64;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};

use backoff::Backoff;
use brokers::{drawn_start, Brokers};
use session::Session;
pub use settings::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, MAX_DRAINER_WAIT_MS, MAX_WAIT_MS,
    MAX_WINDOW,
};
use tally::Tally;

impl RabbitMqCdrSettings {
    /// The writer these settings describe, publishing the default
    /// [`JsonRecordEncoder`] record and recording into `metrics` (the registry
    /// the core exports, so `cdr_written_total` and `cdr_dropped_total` count
    /// its publishes). Every default entry point of the kit comes through here.
    pub fn into_writer(self, metrics: &B2buaMetrics) -> RabbitMqCdrWriter {
        self.into_writer_with_encoder(Arc::new(JsonRecordEncoder), metrics)
    }

    /// [`Self::into_writer`] publishing the bytes `encoder` produces.
    pub fn into_writer_with_encoder(
        self,
        encoder: Arc<dyn CdrEncoder>,
        metrics: &B2buaMetrics,
    ) -> RabbitMqCdrWriter {
        RabbitMqCdrWriter::new(self, encoder, metrics.clone())
    }
}

/// The RabbitMQ writer the settings read through `get` select, publishing the
/// default record and recording into `metrics`; `Ok(None)` when no URL is set.
pub fn rabbitmq_cdr_writer_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    metrics: &B2buaMetrics,
) -> Result<Option<RabbitMqCdrWriter>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| s.into_writer(metrics)))
}

/// [`rabbitmq_cdr_writer_from_lookup`] publishing the bytes `encoder` produces.
pub fn rabbitmq_cdr_writer_from_lookup_with_encoder(
    get: impl Fn(&str) -> Option<String>,
    encoder: Arc<dyn CdrEncoder>,
    metrics: &B2buaMetrics,
) -> Result<Option<RabbitMqCdrWriter>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| s.into_writer_with_encoder(encoder, metrics)))
}

/// Publishes terminated-call CDRs, encoded by its [`CdrEncoder`], onto an AMQP
/// queue under publisher confirms (the module's contract).
pub struct RabbitMqCdrWriter {
    settings: RabbitMqCdrSettings,
    encoder: Arc<dyn CdrEncoder>,
    /// The current connection and the backoff guarding the next one; taken by
    /// the one drainer, so uncontended.
    link: Mutex<Link>,
    /// Counts each record's end: the shared b2bua registry's
    /// `cdr_written_total` / `cdr_dropped_total`, and the observer.
    tally: Tally,
    /// The close deadline; `None` while the writer publishes. Every session's
    /// confirm tracker waits no later than it.
    close_by: watch::Sender<Option<Instant>>,
}

struct Link {
    /// `None` until the first connect, and after a session ended.
    session: Option<Session>,
    /// The confirm trackers of ended sessions still resolving their
    /// publishes, joined by `close`.
    retired: Vec<JoinHandle<()>>,
    backoff: Backoff,
    brokers: Brokers,
}

impl RabbitMqCdrWriter {
    /// The writer of `settings`, its first connect attempt starting at a
    /// broker drawn at random, so the workers of a cluster spread over its
    /// nodes ([`Self::starting_at`] fixes it).
    pub fn new(
        settings: RabbitMqCdrSettings,
        encoder: Arc<dyn CdrEncoder>,
        metrics: B2buaMetrics,
    ) -> Self {
        let bounds = &settings.bounds;
        let backoff = Backoff::new(bounds.backoff_min, bounds.backoff_max);
        // A parked answer lives through the longest gap between two attempts
        // while records flow.
        let max_age = bounds.connect_timeout + bounds.backoff_max;
        let brokers = Brokers::new(&settings.brokers, drawn_start(), max_age);
        Self {
            settings,
            encoder,
            link: Mutex::new(Link { session: None, retired: Vec::new(), backoff, brokers }),
            tally: Tally::new(metrics),
            close_by: watch::Sender::new(None),
        }
    }

    /// This writer's first connect attempt starting at the broker of index
    /// `first` in [`RabbitMqCdrSettings::brokers`] (modulo their count), in
    /// place of the drawn one: a runner that numbers its workers may spread
    /// them by that number.
    pub fn starting_at(mut self, first: usize) -> Self {
        self.link.get_mut().brokers.start_at(first);
        self
    }

    /// This writer telling `observer` the end of each record it is handed:
    /// written on the broker's ack, else dropped, with the time since `write`
    /// took the record. A confirmed record's end is told once the confirms of
    /// the publishes before it are resolved.
    pub fn observed_by(mut self, observer: Arc<dyn CdrDeliveryObserver>) -> Self {
        self.tally.observed_by(observer);
        self
    }

    /// The settings this writer publishes under.
    pub fn settings(&self) -> &RabbitMqCdrSettings {
        &self.settings
    }

    /// The bytes published for `call`; `None` when the encoder refuses it, a
    /// record the caller counts dropped.
    fn payload(&self, call: &Call, terminated_at: i64) -> Option<Vec<u8>> {
        match self.encoder.encode(call, terminated_at) {
            Ok(payload) => Some(payload),
            Err(e) => {
                tracing::warn!(call_ref = %call.call_ref, error = %e, "CDR encode failed");
                None
            }
        }
    }

    /// The live session, connecting one within the connect bound when there
    /// is none and the backoff allows; `None` when no session can publish now.
    /// A session whose connection or channel is no longer open is ended and
    /// replaced, so a connection that died between records costs no record.
    async fn session<'a>(&self, link: &'a mut Link) -> Option<&'a Session> {
        if let Some(current) = &link.session {
            current.end_if_disconnected();
        }
        if let Some(ended) = link.session.take_if(|s| s.ended_at().is_some()) {
            link.brokers.ended(ended.delivered());
            if ended.delivered() {
                link.backoff.recovered();
            } else if let Some(ended_at) = ended.ended_at() {
                // The backoff counts from the end, not from this record.
                link.backoff.failed(ended_at);
            }
            link.retired.retain(|tracker| !tracker.is_finished());
            link.retired.extend(ended.into_tracker());
        }
        if link.session.is_none() {
            if !link.backoff.ready(Instant::now()) {
                return None;
            }
            let deadline = Instant::now() + self.settings.bounds.connect_timeout;
            let attempt = link.brokers.open(&self.settings, deadline, &self.tally, &self.close_by);
            let opened = timeout_at(deadline, attempt).await;
            match opened {
                Ok(Ok(session)) => link.session = Some(session),
                Ok(Err(e)) => {
                    let wait = link.backoff.failed(Instant::now());
                    tracing::warn!(error = %e, retry_in_ms = millis_u64(wait), "CDR broker connect failed");
                    return None;
                }
                Err(_) => {
                    let wait = link.backoff.failed(Instant::now());
                    tracing::warn!(
                        timeout_ms = millis_u64(self.settings.bounds.connect_timeout),
                        retry_in_ms = millis_u64(wait),
                        "CDR broker connect timed out"
                    );
                    return None;
                }
            }
        }
        link.session.as_ref()
    }
}

#[async_trait]
impl CdrWriter for RabbitMqCdrWriter {
    /// A record the encoder refuses is one dropped record, never a stopped
    /// drainer.
    async fn write(&self, call: &Call, terminated_at: i64) {
        let handed_at = Instant::now();
        let Some(payload) = self.payload(call, terminated_at) else {
            return self.tally.dropped(handed_at);
        };
        let mut link = self.link.lock().await;
        if self.close_by.borrow().is_some() {
            return self.tally.dropped(handed_at);
        }
        let Some(session) = self.session(&mut link).await else {
            return self.tally.dropped(handed_at);
        };
        let deadline = Instant::now() + self.settings.bounds.publish_timeout;
        let published = session.publish(&self.settings.queue, &payload, handed_at, deadline);
        if let Err(refused) = published.await {
            self.tally.dropped(handed_at);
            tracing::debug!(?refused, "CDR record dropped");
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        // Not a test sink — records live in the broker, not in process memory.
        Vec::new()
    }

    /// A write still holding the link at `deadline` keeps its session: its
    /// tracker counts that session's publishes by the deadline all the same.
    async fn close(&self, deadline: Instant) {
        self.close_by.send_if_modified(|by| {
            let first = by.is_none();
            if first {
                *by = Some(deadline);
            }
            first
        });
        let Ok(mut link) = timeout_at(deadline, self.link.lock()).await else {
            return;
        };
        if let Some(session) = link.session.take() {
            session.close().await;
        }
        for tracker in std::mem::take(&mut link.retired) {
            let _ = tracker.await;
        }
    }
}

#[cfg(test)]
mod tests;
