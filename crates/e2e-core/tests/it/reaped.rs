//! The fake infra's reaped check: a call the shape leaves up fails the run as
//! a gating anomaly on its report, the diagram and findings kept.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use e2e_core::model::Input;
use e2e_core::{EndpointConfig, FakeLsbcB2bua, InfraShape};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn fake_cfg() -> EndpointConfig {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
        egress: None,
    }
}

/// The subject is the check itself: the call is left established on purpose.
#[tokio::test(start_paused = true)]
async fn a_call_left_up_fails_the_report_as_a_gating_anomaly() {
    let rt = FakeLsbcB2bua.build("reaped/left-up", &fake_cfg()).await;
    let (alice, bob1) = (rt.agent("alice"), rt.agent("bob1"));
    let invite =
        rt.outgoing_invite(&["bob1"], &Input::default(), alice.invite(bob1).with_sdp(OFFER));
    let mut call = invite.send().await;
    bob1.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob1.receive("ACK").await;

    let (report, _rfc_gate) = rt.finish().await;
    let leak = report
        .extra_anomalies
        .iter()
        .find(|a| a.check == "sut.fullyReaped")
        .expect("the reaped check's anomaly");
    assert_eq!(leak.advisory, Some(false), "a leak gates");
    assert!(leak.detail.contains("call leak"), "{}", leak.detail);
    assert!(!report.passed(), "the run fails");
    assert!(!report.entries().is_empty(), "the diagram is kept");
}
