use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::*;
use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;

fn a_call() -> Call {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-buf\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: buf@10.0.0.9\r\n\
        CSeq: 1 INVITE\r\n\
        Contact: <sip:alice@10.0.0.9:5060>\r\n\
        Content-Length: 0\r\n\r\n";
    let req = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    };
    let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(
        &req,
        std::net::SocketAddr::from(([10, 0, 0, 9], 5060)),
        &cfg,
        &mut sip_ids::TurnIds::fixed(1),
        1_000,
    )
}

/// Takes `per_record` to write each record; remembers when it was closed.
struct SlowSink {
    per_record: Duration,
    written: InMemoryCdrWriter,
    closed_by: std::sync::Mutex<Option<Instant>>,
}

impl SlowSink {
    fn new(per_record: Duration) -> Arc<Self> {
        Arc::new(Self {
            per_record,
            written: InMemoryCdrWriter::new(),
            closed_by: Default::default(),
        })
    }

    fn written(&self) -> usize {
        self.written.snapshot().len()
    }
}

#[async_trait]
impl CdrWriter for SlowSink {
    async fn write(&self, call: &Call, terminated_at: i64) {
        tokio::time::sleep(self.per_record).await;
        self.written.write(call, terminated_at).await;
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.written.read_all().await
    }

    async fn close(&self, deadline: Instant) {
        *self.closed_by.lock().unwrap() = Some(deadline);
    }
}

async fn buffer_of(n: usize, sink: &Arc<SlowSink>, metrics: &B2buaMetrics) -> BufferedCdrWriter {
    let buf = BufferedCdrWriter::spawn(sink.clone(), 64, metrics.clone());
    for _ in 0..n {
        buf.write(&a_call(), 2_000).await;
    }
    buf
}

#[tokio::test(start_paused = true)]
async fn close_writes_every_queued_record_then_closes_the_inner_writer() {
    let (sink, metrics) = (SlowSink::new(Duration::from_millis(100)), B2buaMetrics::new());
    let buf = buffer_of(5, &sink, &metrics).await;
    let deadline = Instant::now() + Duration::from_secs(3);
    buf.close(deadline).await;
    assert_eq!(sink.written(), 5);
    assert_eq!(*sink.closed_by.lock().unwrap(), Some(deadline), "inner closed, same deadline");
    assert_eq!((buf.dropped_total(), metrics.cdr_dropped_total()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn the_deadline_cuts_the_close_and_counts_every_record_left_dropped() {
    let (sink, metrics) = (SlowSink::new(Duration::from_secs(1)), B2buaMetrics::new());
    let buf = buffer_of(10, &sink, &metrics).await;
    let deadline = Instant::now() + Duration::from_millis(2_500);
    buf.close(deadline).await;
    assert_eq!(Instant::now(), deadline, "the close returns at its deadline");
    assert_eq!(sink.written(), 2, "two writes fit before the deadline");
    assert_eq!(metrics.cdr_dropped_total(), 8, "the write cut at the deadline and the queued 7");
    assert_eq!(buf.dropped_total(), 8);
}

#[tokio::test(start_paused = true)]
async fn a_write_after_the_close_is_counted_dropped() {
    let (sink, metrics) = (SlowSink::new(Duration::ZERO), B2buaMetrics::new());
    let buf = buffer_of(1, &sink, &metrics).await;
    buf.close(Instant::now() + Duration::from_secs(1)).await;
    buf.write(&a_call(), 3_000).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(sink.written(), 1);
    assert_eq!(metrics.cdr_dropped_total(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_second_close_waits_for_the_first() {
    let (sink, metrics) = (SlowSink::new(Duration::from_millis(100)), B2buaMetrics::new());
    let buf = buffer_of(3, &sink, &metrics).await;
    let deadline = Instant::now() + Duration::from_secs(3);
    let (first, second) = (buf.clone(), buf.clone());
    tokio::join!(first.close(deadline), second.close(deadline + Duration::from_secs(9)));
    assert_eq!(sink.written(), 3);
    assert_eq!(*sink.closed_by.lock().unwrap(), Some(deadline), "the first deadline holds");
}

#[tokio::test(start_paused = true)]
async fn a_passthrough_close_closes_the_inner_writer() {
    let sink = SlowSink::new(Duration::ZERO);
    let buf = BufferedCdrWriter::spawn(sink.clone(), 0, B2buaMetrics::new());
    let deadline = Instant::now() + Duration::from_secs(1);
    buf.close(deadline).await;
    assert_eq!(*sink.closed_by.lock().unwrap(), Some(deadline));
}
