//! A runtime shut down while the RabbitMQ CDR sink connects panics no thread
//! of the AMQP client: no tokio timer runs on the client's IO thread.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{CdrEncodeError, CdrEncoder, CdrWriter};
use b2bua::metrics::B2buaMetrics;
use b2bua_runner_kit::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
};
use call::Call;

use crate::support::fake_broker::{Admit, FakeBroker, Reply};

struct Fixed;

impl CdrEncoder for Fixed {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(b"cdr".to_vec())
    }
}

/// Counts the panics of threads the AMQP client names as its IO loop.
fn count_io_loop_panics() -> Arc<AtomicUsize> {
    let panics = Arc::new(AtomicUsize::new(0));
    let seen = panics.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("lapin-io-loop") {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        previous(info);
    }));
    panics
}

/// A listener whose zero backlog one connection fills: every further connect
/// hangs until its own deadline.
fn black_holed() -> (socket2::Socket, std::net::TcpStream, SocketAddr) {
    use socket2::{Domain, Socket, Type};
    let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    listener.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
    listener.listen(0).unwrap();
    let addr = listener.local_addr().unwrap().as_socket().unwrap();
    let filler = std::net::TcpStream::connect(addr).unwrap();
    let blocked = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200));
    assert!(blocked.is_err(), "the full backlog must drop a further SYN");
    (listener, filler, addr)
}

fn writer_to(addr: SocketAddr) -> RabbitMqCdrWriter {
    let settings = RabbitMqCdrSettings {
        url: format!("amqp://guest:guest@{addr}/%2f"),
        queue: "cdr".into(),
        declare: CdrQueueDeclare::Own { max_len: 0 },
        bounds: CdrDeliveryBounds { connect_timeout: Duration::from_secs(5), ..Default::default() },
    };
    RabbitMqCdrWriter::new(settings, Arc::new(Fixed), B2buaMetrics::new())
}

/// Starts one write to `addr` on a fresh runtime, then drops the runtime
/// while the write is still connecting.
fn drop_the_runtime_mid_connect(addr: SocketAddr) {
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let call = crate::cdr_rabbitmq_delivery::a_call();
    rt.spawn(async move {
        writer_to(addr).write(&call, 2_000).await;
    });
    std::thread::sleep(Duration::from_millis(200));
    drop(rt);
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn a_runtime_dropped_during_the_tcp_connect_panics_no_client_thread() {
    let panics = count_io_loop_panics();
    let (_listener, _filler, hole) = black_holed();
    drop_the_runtime_mid_connect(hole);
    assert_eq!(panics.load(Ordering::SeqCst), 0, "an AMQP client IO thread panicked");
}

#[test]
fn a_runtime_dropped_during_the_amqp_handshake_panics_no_client_thread() {
    let panics = count_io_loop_panics();
    let setup = tokio::runtime::Runtime::new().unwrap();
    let broker = setup.block_on(async {
        let broker = FakeBroker::start(Reply::Ack).await;
        broker.admit(Admit::StallHandshake);
        broker
    });
    drop_the_runtime_mid_connect(broker.addr);
    drop(setup);
    assert_eq!(panics.load(Ordering::SeqCst), 0, "an AMQP client IO thread panicked");
}
