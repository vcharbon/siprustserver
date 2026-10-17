//! The discovered pool over a kube client that fails to build first, on the
//! paused clock. The client that finally builds talks to a stand-in API
//! server: it lists one ready worker and holds the watch open.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{Either, Full, StreamBody};
use probe_http::ProbeState;
use sip_proxy::registry::WorkerHealth;

use super::*;
use crate::readiness::probe_state;

type Frame = Result<http_body::Frame<Bytes>, Infallible>;
type Body = Either<Full<Bytes>, StreamBody<futures::stream::Pending<Frame>>>;

/// The EndpointSlice list the stand-in API server answers: one ready
/// endpoint, pod `worker-0` at 10.0.0.7.
fn slice_list() -> Bytes {
    let list = serde_json::json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSliceList",
        "metadata": { "resourceVersion": "1" },
        "items": [{
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": "workers-abcde",
                "namespace": "lab",
                "resourceVersion": "1",
                "labels": { "kubernetes.io/service-name": "workers" }
            },
            "addressType": "IPv4",
            "endpoints": [{
                "addresses": ["10.0.0.7"],
                "conditions": { "ready": true },
                "targetRef": { "kind": "Pod", "name": "worker-0", "namespace": "lab" }
            }]
        }]
    });
    Bytes::from(list.to_string())
}

/// A kube client whose API server lists [`slice_list`] and never ends a watch.
fn stand_in_client() -> kube::Client {
    let api = tower::service_fn(|req: http::Request<kube::client::Body>| async move {
        let watching = req.uri().query().is_some_and(|q| q.contains("watch=true"));
        let body: Body = if watching {
            Either::Right(StreamBody::new(futures::stream::pending()))
        } else {
            Either::Left(Full::new(slice_list()))
        };
        Ok::<_, Infallible>(http::Response::new(body))
    });
    kube::Client::new(api, "lab")
}

/// A fixed pool is listed at once and reads every worker `Unknown` until it
/// answers a probe.
#[test]
fn a_fixed_pool_is_listed_with_every_worker_unknown() {
    let pool = static_pool("w0@10.0.0.1:5070,w1@10.0.0.2:5070", Clock::test_at(0)).unwrap();
    assert!(pool.registry.synced());
    let health: Vec<WorkerHealth> = pool.registry.snapshot().iter().map(|w| w.health).collect();
    assert_eq!(health, [WorkerHealth::Unknown, WorkerHealth::Unknown]);
}

/// No kube client at boot: the pool is empty and the proxy NotReady, with no
/// stand-in worker; the client is built again, the watch lists the worker,
/// and the proxy turns Ready once that worker answers OPTIONS.
#[tokio::test(start_paused = true)]
async fn a_kube_client_that_fails_then_builds_fills_the_pool_and_never_fakes_a_worker() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let client = stand_in_client();
    let connect = move || {
        let attempt = counted.fetch_add(1, Ordering::SeqCst);
        let client = client.clone();
        async move {
            match attempt {
                0 => Err("no service account token".to_string()),
                _ => Ok(client),
            }
        }
    };
    let pool =
        discovered_pool_with(connect, "lab".into(), "workers".into(), 5060, Clock::test_at(0));

    assert!(
        pool.registry.snapshot().is_empty(),
        "no worker before the watch lists one: {:?}",
        pool.registry.snapshot()
    );
    assert_eq!(probe_state(false, false, true, pool.registry.as_ref()), ProbeState::NotReady);
    assert!(!pool.registry.synced(), "no listing yet: the probe's first round waits");
    let watch = pool.discovery.as_ref().expect("a discovered pool");
    assert!(!watch.watch_health().client_built);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "the client is built again after 1 s");
    assert!(watch.watch_health().client_built);
    assert!(pool.registry.synced(), "the first listing is in");
    let listed: Vec<String> =
        pool.registry.snapshot().iter().map(|w| format!("{}@{}", w.id, w.address)).collect();
    assert_eq!(listed, ["worker-0@10.0.0.7:5060"]);
    assert_eq!(
        probe_state(false, false, true, pool.registry.as_ref()),
        ProbeState::NotReady,
        "listed but not yet answering OPTIONS"
    );

    pool.control.set_health("worker-0", WorkerHealth::Alive);
    assert_eq!(probe_state(false, false, true, pool.registry.as_ref()), ProbeState::Ready);
}

/// The first watch fails to start (500), the next one starts and stays quiet:
/// a re-started watch delivers no event, so the broken episode ends after the
/// quiet period with no error, and not before.
#[tokio::test(start_paused = true)]
async fn a_watch_that_restarts_quietly_ends_its_broken_episode() {
    let watches = Arc::new(AtomicUsize::new(0));
    let counted = watches.clone();
    let api = tower::service_fn(move |req: http::Request<kube::client::Body>| {
        let counted = counted.clone();
        async move {
            let watching = req.uri().query().is_some_and(|q| q.contains("watch=true"));
            let mut status = http::StatusCode::OK;
            let body: Body = if !watching {
                Either::Left(Full::new(slice_list()))
            } else if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                status = http::StatusCode::INTERNAL_SERVER_ERROR;
                Either::Left(Full::new(Bytes::from_static(
                    b"{\"kind\":\"Status\",\"apiVersion\":\"v1\",\"metadata\":{},\"status\":\"Failure\",\"message\":\"boom\",\"reason\":\"InternalError\",\"code\":500}",
                )))
            } else {
                Either::Right(StreamBody::new(futures::stream::pending()))
            };
            let mut resp = http::Response::new(body);
            *resp.status_mut() = status;
            Ok::<_, Infallible>(resp)
        }
    });
    let client = kube::Client::new(api, "lab");
    let connect = move || {
        let client = client.clone();
        async move { Ok::<_, String>(client) }
    };
    let pool =
        discovered_pool_with(connect, "lab".into(), "workers".into(), 5060, Clock::test_at(0));
    let watch = pool.discovery.as_ref().expect("a discovered pool");

    tokio::time::sleep(Duration::from_secs(60)).await;
    let n = watches.load(Ordering::SeqCst);
    assert!(n >= 2, "re-watched: {n} watch requests");
    assert!(watch.watch_health().broken_since_unix_ms.is_some(), "still within the quiet period");

    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(
        watch.watch_health().broken_since_unix_ms,
        None,
        "the re-started watch has been quiet past the quiet period"
    );
}
