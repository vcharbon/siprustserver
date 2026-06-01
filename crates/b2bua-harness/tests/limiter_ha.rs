//! HA-coupled limiter: a call admitted on the primary replicates its limiter
//! hold to the backup; when the primary crashes and the in-dialog BYE fails over
//! to the backup, the backup hydrates the call (holds included) and releases the
//! hold on termination — so the shared counter drains on the **takeover** node
//! (decrement-via-backup-BYE / decrement-after-respawn).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{FailoverHarness, ReplicatedB2buaSut, WorkerHealth};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// w_pri ordinal from the proxy's Record-Route stickiness cookie.
fn pri_from_cookie(rr: &str) -> String {
    let params = sip_message::message_helpers::parse_uri_params(rr);
    params.get("w_pri").cloned().unwrap_or_default()
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry {
                    id: "trunk-A".into(),
                    limit: 1,
                }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn hold_is_released_on_the_takeover_node_after_primary_crash() {
    let mut fh = FailoverHarness::new("limiter-ha-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;

    // Both workers share the limiter server + the limiter-carrying decision.
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // alice INVITEs through the proxy; establish the call.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg carries the proxy cookie")
        .to_string();
    let primary_ord = pri_from_cookie(&rr);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Drive establish + replicate primary → backup.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "call admitted: one hold");

    // Bind primary/backup by the cookie's w_pri.
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" {
            (&mut w_b1, &mut w_b2)
        } else {
            (&mut w_b2, &mut w_b1)
        };

    // Crash the primary; mark it dead so the proxy fails the in-dialog request
    // over to the backup.
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // alice BYEs: the backup hydrates the replicated call (limiter hold included)
    // and tears it down — releasing the hold from the takeover node.
    let creations_before = backup.metrics().creations_total();
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // Let the takeover handling + the soft release drain.
    let mut released = false;
    for _ in 0..40 {
        fh.advance(Duration::from_millis(200)).await;
        if store.stats().current_total == 0 {
            released = true;
            break;
        }
    }
    assert!(
        backup.metrics().creations_total() > creations_before,
        "backup processed the failed-over BYE",
    );
    assert!(released, "the takeover node released the limiter hold on BYE");

    let _ = fh.report().await;
}
