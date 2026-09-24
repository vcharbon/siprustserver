//! Every connection the RabbitMQ CDR sink abandons ends its client IO thread
//! and its tasks: a broker that stalls the handshake or never confirms leaves
//! no thread, task or socket behind, however many times the sink reconnects. One test per
//! binary, so no other test's connections are counted.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{CdrEncodeError, CdrEncoder, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::metrics::B2buaMetrics;
use b2bua_runner_kit::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
};
use call::Call;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use support::fake_broker::{Admit, FakeBroker, Reply};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);

fn a_call() -> Call {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-io\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: sink-io@10.0.0.9\r\n\
        CSeq: 1 INVITE\r\n\
        Contact: <sip:alice@10.0.0.9:5060>\r\n\
        Content-Length: 0\r\n\r\n";
    let req = match CustomParser::new().parse(raw.as_bytes()).expect("parse") {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    };
    let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(&req, SocketAddr::from(([10, 0, 0, 9], 5060)), &cfg, 1_000)
}

struct Fixed;

impl CdrEncoder for Fixed {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(b"cdr".to_vec())
    }
}

/// Threads of this process named as the AMQP client names its IO loop.
fn io_loop_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("procfs")
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "lapin-io-loop")
        .count()
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current().metrics().num_alive_tasks()
}

async fn alive_tasks_reach(want: usize) -> usize {
    let give_up = Instant::now() + Duration::from_secs(3);
    let mut n = alive_tasks();
    while n != want && Instant::now() < give_up {
        tokio::time::sleep(10 * MS).await;
        n = alive_tasks();
    }
    n
}

async fn io_loop_threads_reach(want: usize) -> usize {
    let give_up = Instant::now() + Duration::from_secs(3);
    let mut n = io_loop_threads();
    while n != want && Instant::now() < give_up {
        tokio::time::sleep(10 * MS).await;
        n = io_loop_threads();
    }
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_connections_leave_no_io_thread_nor_task_behind() {
    let broker = FakeBroker::start(Reply::Silent).await;
    // The broker's accept loop is the only task before the sink runs.
    let baseline = alive_tasks();
    let bounds = CdrDeliveryBounds {
        window: 8,
        connect_timeout: 150 * MS,
        publish_timeout: 100 * MS,
        confirm_timeout: 150 * MS,
        backoff_min: 20 * MS,
        backoff_max: 20 * MS,
    };
    let settings = RabbitMqCdrSettings {
        url: format!("amqp://guest:guest@{}/%2f", broker.addr),
        queue: "cdr".into(),
        declare: CdrQueueDeclare::Own { max_len: 0 },
        bounds,
    };
    let metrics = B2buaMetrics::new();
    let w = RabbitMqCdrWriter::new(settings, Arc::new(Fixed), metrics.clone());
    let call = a_call();
    assert_eq!(io_loop_threads(), 0);

    // Never confirmed: each connection ends at the confirm bound.
    for _ in 0..6 {
        w.write(&call, 2_000).await;
        tokio::time::sleep(200 * MS).await;
    }
    // Handshake never answered: each connect ends at the connect bound.
    broker.admit(Admit::StallHandshake);
    for _ in 0..6 {
        w.write(&call, 2_000).await;
        tokio::time::sleep(30 * MS).await;
    }
    assert!(broker.count(|c| &c.accepted) >= 12);
    assert!(io_loop_threads_reach(0).await <= 1, "only the last attempt may still be live");
    drop(w);
    assert_eq!(io_loop_threads_reach(0).await, 0, "every abandoned connection's thread ended");
    assert_eq!(alive_tasks_reach(baseline).await, baseline, "no session leaves a task behind");
    assert_eq!(metrics.cdr_written_total(), 0);
    assert_eq!(metrics.cdr_dropped_total(), 12);
}
