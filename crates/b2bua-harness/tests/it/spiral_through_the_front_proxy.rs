//! A spiral (RFC 3261 §16.3) that crosses the load-balancing front proxy both
//! ways: the B2BUA's outgoing INVITE leaves through the front proxy to a
//! third-party stateful proxy, which sends it back through the front proxy to
//! the same B2BUA as a new call.
//!
//!   alice ─▶ LB ─▶ b2bua (call 1) ─▶ LB ─▶ proxy ─▶ LB ─▶ b2bua (call 2) ─▶ LB ─▶ bob
//!
//! The front proxy sees two INVITEs under one Call-ID, From tag and CSeq: call
//! 1's outgoing one from the B2BUA and the spiralled one from the third party,
//! each on its own branch. Each CANCEL belongs with its own INVITE (§9.1): call
//! 1's CANCEL of its outgoing leg reaches the third party, whose CANCEL of its
//! forwarded INVITE reaches call 2 (§16.10).

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::CdrEventType;
use scenario_harness::callflow::OFFER_SDP;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::common;
use crate::common::stateful_proxy::spawn_stateful_proxy_toward;

const PROXY_PORT: u16 = 5090;
const LB_PORT: u16 = 5100;
/// The Request-URI call 1 sends to the third party; coming back, it names
/// call 2's route.
const SPIRAL_USER: &str = "spiral";

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Call 1 (alice's) goes to the third party; call 2 (the spiralled
/// Request-URI) goes to bob.
fn spiral_decision(bob_port: u16) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .on(move |req| {
                req.ruri.starts_with(&format!("sip:{SPIRAL_USER}@")).then(|| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.new_ruri = Some(format!("sip:bob@127.0.0.1:{bob_port}"));
                    NewCallResponse::Route(r)
                })
            })
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", PROXY_PORT);
                r.new_ruri = Some(format!("sip:{SPIRAL_USER}@127.0.0.1:{PROXY_PORT}"));
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Whether the recording holds a `method` request sent `from → to`.
fn crossed(report: &scenario_harness::RunReport, from: u16, to: u16, method: &str) -> bool {
    report.entries().iter().any(|e| {
        e.from == addr(from)
            && e.to == addr(to)
            && matches!(
                CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Request(r)) if r.method() == method
            )
    })
}

/// alice CANCELs while bob rings: call 1 CANCELs its outgoing leg through the
/// front proxy to the third party, the third party CANCELs its forwarded INVITE
/// through the front proxy to call 2, and call 2 CANCELs bob. Every INVITE
/// ends 487.
#[tokio::test(start_paused = true)]
async fn a_spiral_through_the_front_proxy_cancelled_by_the_caller_while_ringing() {
    let s = B2buaScene::with_b2bua("spiral-front-proxy-cancelled", |bob_port| {
        B2buaSut::builder(spiral_decision(bob_port)).outbound_proxy("127.0.0.1", LB_PORT)
    })
    .await;
    let lb =
        common::spawn_lb_proxy(&s.h, &format!("127.0.0.1:{LB_PORT}"), "b2bua", s.b2bua.addr).await;
    let _proxy = spawn_stateful_proxy_toward(
        &s.h,
        "third-party",
        &format!("127.0.0.1:{PROXY_PORT}"),
        lb.addr(),
    )
    .await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(lb.addr()).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut b_cxl =
        s.bob.try_receive("CANCEL").await.expect("the CANCEL crosses the spiral to bob");
    b_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    uas.expect_ack().await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    for cdr in &cdrs {
        let k: Vec<_> = cdr.events.iter().map(|e| e.event_type).collect();
        assert!(!k.contains(&CdrEventType::Answer), "neither call is answered: {k:?}");
    }
    let b2bua_port = s.b2bua.addr.port();
    let report = s.finish().await;
    assert!(
        crossed(&report, LB_PORT, PROXY_PORT, "CANCEL"),
        "call 1's CANCEL reaches the third party, where its INVITE went"
    );
    assert!(
        crossed(&report, PROXY_PORT, LB_PORT, "CANCEL"),
        "the third party CANCELs the INVITE it forwarded"
    );
    assert!(crossed(&report, LB_PORT, b2bua_port, "CANCEL"), "call 2 is CANCELled");
}
