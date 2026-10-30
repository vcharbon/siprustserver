//! `ExternalHttp` on the simulated fabric of a paused-clock test, forwarding
//! to a real server on loopback that answers after real time: the answer lands
//! at its virtual instant, and a backend that never answers in time or is gone
//! is an environment fault and a closed connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clock_bridge::{ClockMode, EnvFault, RealSide};
use http_net::failures::FailureCause;
use http_net::{HttpRequest, HttpResponse, HttpService, HttpTransport, RealHttpNetwork};
use http_net_testkit::{ExternalHttp, SimulatedHttpNetwork};
use tokio::time::Instant;

const SIM_ADDR: &str = "10.0.0.1:80";
const LATENCY: Duration = Duration::from_millis(10);

/// A real backend answering `200 <path>` after the milliseconds its path names.
struct Slow;

#[async_trait]
impl HttpService for Slow {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let ms: u64 = req.path.trim_start_matches('/').parse().unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        HttpResponse::ok(req.path.into_bytes())
    }
}

/// The real backend on a runtime of its own (real clock), for the process's life.
fn real_backend() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let server = RealHttpNetwork::new()
                .serve("127.0.0.1:0".parse().unwrap(), Arc::new(Slow))
                .await
                .unwrap();
            tx.send(server.local_addr()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    rx.recv().unwrap()
}

async fn fabric(
    side: &Arc<RealSide>,
    upstream: SocketAddr,
) -> (SimulatedHttpNetwork, Box<dyn http_net::HttpServerHandle>) {
    let net = SimulatedHttpNetwork::new();
    let svc = ExternalHttp::new(side.clone(), Arc::new(RealHttpNetwork::new()), upstream, LATENCY);
    let handle = net.serve(SIM_ADDR.parse().unwrap(), Arc::new(svc)).await.unwrap();
    (net, handle)
}

#[tokio::test(start_paused = true)]
async fn a_real_answer_lands_at_its_virtual_instant() {
    let side = RealSide::new(ClockMode::Paused, Duration::from_millis(500));
    let (net, _h) = fabric(&side, real_backend()).await;
    let start = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        net.request(SIM_ADDR.parse().unwrap(), HttpRequest::get("/40")),
    )
    .await
    .expect("the answer, not the client's 2 s timeout")
    .unwrap();
    assert_eq!(resp.body, b"/40");
    let transit = start.elapsed() - LATENCY;
    assert!(
        transit < Duration::from_millis(5),
        "only the fabric's transit beside the latency: {transit:?}"
    );
    assert!(side.faults().faults().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_backend_past_the_cap_closes_the_connection_and_is_a_stall() {
    let side = RealSide::new(ClockMode::Paused, Duration::from_millis(50));
    let (net, _h) = fabric(&side, real_backend()).await;
    let err = net.request(SIM_ADDR.parse().unwrap(), HttpRequest::get("/300")).await.unwrap_err();
    assert_eq!(err.cause, FailureCause::Eof);
    assert!(
        matches!(&side.faults().faults()[0], EnvFault::Stall { label, .. } if label.contains("GET /300"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_gone_backend_is_an_external_failure() {
    let side = RealSide::new(ClockMode::Paused, Duration::from_millis(500));
    let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (net, _h) = fabric(&side, gone).await;
    let err = net.request(SIM_ADDR.parse().unwrap(), HttpRequest::get("/0")).await.unwrap_err();
    assert_eq!(err.cause, FailureCause::Eof);
    assert!(
        matches!(&side.faults().faults()[0], EnvFault::Failed { .. }),
        "{:?}",
        side.faults().faults()
    );
}
