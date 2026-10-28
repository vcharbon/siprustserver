//! A dead verdict reaches every other alive worker, signed; a departure, a
//! worker read alive and the dead worker itself are told nothing.

use std::time::Duration;

use sip_message::dead_notice::dead_notice;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use super::*;
use crate::addr::ProxyAddr;
use crate::registry::simulated::SimulatedWorkerRegistry;

const NOTIFIER: &str = "192.0.2.250:5061";
const W1: (&str, u16) = ("10.244.5.8", 5060);
const W2: (&str, u16) = ("10.244.5.9", 5060);
const W3: (&str, u16) = ("10.244.5.10", 5060);

fn key() -> SigningKey {
    SigningKey::new(&[3u8; 32]).expect("a 32-byte key")
}

struct Fixture {
    registry: SimulatedWorkerRegistry,
    workers: Vec<Box<dyn UdpEndpoint>>,
    task: tokio::task::JoinHandle<()>,
}

async fn three_alive_workers() -> Fixture {
    let clock = Clock::test_at(1_000);
    let entry =
        |id: &str, (host, port): (&str, u16)| WorkerEntry::alive(id, ProxyAddr::new(host, port));
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![entry("w1", W1), entry("w2", W2), entry("w3", W3)],
        clock.clone(),
    );
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at: String| BindUdpOpts::new(at.parse().unwrap(), 64);
    let mut workers = Vec::new();
    for (host, port) in [W1, W2, W3] {
        workers.push(net.bind_udp(bind(format!("{host}:{port}"))).await.unwrap());
    }
    let ep = net.bind_udp(bind(NOTIFIER.into())).await.unwrap();
    let notifier =
        DeadNotifier::new(ep, Arc::new(registry.clone()), key(), clock, Arc::new(IdGen::seeded(9)));
    let task = tokio::spawn(notifier.run());
    Fixture { registry, workers, task }
}

/// The OPTIONS `worker` received within a second, if any.
async fn received(worker: &dyn UdpEndpoint) -> Option<SipRequest> {
    let packet = tokio::time::timeout(Duration::from_secs(1), worker.recv()).await.ok()??;
    match CustomParser::default().parse(&packet.raw).ok()? {
        SipMessage::Request(req) => Some(req),
        SipMessage::Response(_) => None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_dead_verdict_reaches_every_other_alive_worker_signed() {
    let f = three_alive_workers().await;
    f.registry.set_health("w3", WorkerHealth::Unknown);
    f.registry.set_health("w1", WorkerHealth::Dead);

    let req = received(f.workers[1].as_ref()).await.expect("w2 is told");
    assert_eq!(req.method(), "OPTIONS");
    let notice = &dead_notice(&req).expect("a notice");
    assert_eq!(notice.notice.ordinal, "w1");
    assert_eq!(notice.notice.at_ms, 1_000, "the verdict's instant");
    let fields = notice.notice.signed_fields();
    let signature = notice.signature.as_deref().expect("signed");
    assert!(key().verify(&fields.each_ref().map(Vec::as_slice), signature));

    assert!(received(f.workers[0].as_ref()).await.is_none(), "the dead worker is told nothing");
    assert!(received(f.workers[2].as_ref()).await.is_none(), "an unprobed worker is told nothing");
    f.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_departure_and_a_worker_read_alive_are_no_verdict() {
    let f = three_alive_workers().await;
    f.registry.remove("w1");
    f.registry.set_health("w3", WorkerHealth::Alive);
    f.registry.set_health("w3", WorkerHealth::Unknown);
    assert!(received(f.workers[1].as_ref()).await.is_none(), "nothing is told");
    f.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_worker_already_dead_is_no_new_verdict() {
    let f = three_alive_workers().await;
    f.registry.set_health("w1", WorkerHealth::Dead);
    assert!(received(f.workers[1].as_ref()).await.is_some(), "the first verdict is told");
    f.registry.set_health("w1", WorkerHealth::Dead);
    // The first notice's Timer E repeats carry its own instant.
    let repeat = received(f.workers[1].as_ref()).await.expect("Timer E repeats the notice");
    assert_eq!(
        dead_notice(&repeat).expect("a notice").notice.at_ms,
        1_000,
        "a repeat, not a second verdict"
    );
    f.task.abort();
}
