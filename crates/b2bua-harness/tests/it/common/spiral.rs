//! The spiral scene (RFC 3261 §16.3) shared by the scenarios that drive more
//! than the initial INVITE across it: the B2BUA routes alice's call (call 1)
//! to a third-party proxy with a Request-URI at the B2BUA, and the proxy sends
//! it back as a second call (call 2), routed to bob.
//!
//!   alice ──▶ b2bua (call 1) ──▶ proxy ──▶ b2bua (call 2) ──▶ bob

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use b2bua::admission::Class;
use b2bua::cdr::CdrRecord;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut, B2BUA_PORT};
use call::CdrEventType;
use scenario_harness::RunReport;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::common::stateful_proxy::{
    spawn_forking_proxy, spawn_stateful_proxy_with, RecordRoute, StatefulProxy,
};

/// How many in-dialog `method` requests (with a To tag) the recording holds
/// sent from port `from` to port `to`.
fn requests_between(report: &RunReport, from: u16, to: u16, method: &str) -> usize {
    report
        .entries()
        .iter()
        .filter(|e| e.from.port() == from && e.to.port() == to)
        .filter(|e| {
            matches!(CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Request(r)) if r.method() == method && r.to().tag().is_some())
        })
        .count()
}

/// The in-dialog `method` requests between call 1 and call 2 took the path the proxy's
/// Record-Route choice gives them: through the proxy when it Record-Routes,
/// from the B2BUA to itself when it does not (§16.6 step 4, §12.2.1.1).
pub fn assert_crossed_between_the_calls(
    report: &RunReport,
    record_route: RecordRoute,
    method: &str,
) {
    let through_proxy = requests_between(report, B2BUA_PORT, PROXY_PORT, method);
    let direct = requests_between(report, B2BUA_PORT, B2BUA_PORT, method);
    match record_route {
        RecordRoute::Yes => assert!(
            through_proxy > 0 && direct == 0,
            "a Record-Routing proxy carries every {method} between the calls \
             ({through_proxy} through it, {direct} direct)"
        ),
        RecordRoute::No => assert!(
            through_proxy == 0 && direct > 0,
            "without Record-Route every {method} goes from the B2BUA to itself \
             ({through_proxy} through the proxy, {direct} direct)"
        ),
    }
}

pub const PROXY_PORT: u16 = 5090;
/// Where a forking proxy sends its second branch.
pub const CAROL_PORT: u16 = 5110;
/// The Request-URI call 1 sends to the proxy; coming back to the B2BUA, it
/// names call 2's route.
const SPIRAL_USER: &str = "spiral";

/// Call 1 is routed to the proxy with a Request-URI at the B2BUA; call 2
/// (that Request-URI coming back) is routed to bob.
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
                r.new_ruri = Some(format!("sip:{SPIRAL_USER}@127.0.0.1:{B2BUA_PORT}"));
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// alice, bob and the B2BUA of a [`B2buaScene`], with the third-party proxy
/// between call 1 and call 2.
pub async fn spiral_scene(name: &str, record_route: RecordRoute) -> (B2buaScene, StatefulProxy) {
    let s =
        B2buaScene::with_b2bua(name, |bob_port| B2buaSut::builder(spiral_decision(bob_port))).await;
    let proxy =
        spawn_stateful_proxy_with(&s.h, "proxy", &format!("127.0.0.1:{PROXY_PORT}"), record_route)
            .await;
    (s, proxy)
}

/// [`spiral_scene`] with a proxy that forks call 1's INVITE: one branch to
/// the B2BUA (the spiral), one to carol at [`CAROL_PORT`] (RFC 3261 §16.6).
pub async fn forking_spiral_scene(
    name: &str,
    record_route: RecordRoute,
) -> (B2buaScene, StatefulProxy) {
    let s =
        B2buaScene::with_b2bua(name, |bob_port| B2buaSut::builder(spiral_decision(bob_port))).await;
    let proxy = spawn_forking_proxy(
        &s.h,
        "proxy",
        &format!("127.0.0.1:{PROXY_PORT}"),
        record_route,
        ("carol", SocketAddr::from(([127, 0, 0, 1], CAROL_PORT))),
    )
    .await;
    (s, proxy)
}

/// The CDR event kinds of `cdr`, in order.
pub fn kinds(cdr: &CdrRecord) -> Vec<CdrEventType> {
    cdr.events.iter().map(|e| e.event_type).collect()
}

/// Two CDRs: call 1 under alice's identity, call 2 under the identity of the
/// call 1 leg that went to the proxy, each with its one outgoing leg.
pub fn assert_two_calls(s: &B2buaScene, alice_call_id: &str) -> (CdrRecord, CdrRecord) {
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "one CDR per call: {cdrs:?}");
    let first = cdrs
        .iter()
        .find(|c| c.a_leg.call_id == alice_call_id)
        .expect("call 1 is recorded under alice's Call-ID")
        .clone();
    assert_eq!(first.b_legs.len(), 1, "call 1 has its one outgoing leg");
    let looped = &first.b_legs[0].call_id;
    let second = cdrs
        .iter()
        .find(|c| &c.a_leg.call_id == looped)
        .expect("call 2 is recorded under call 1's outgoing Call-ID")
        .clone();
    assert_ne!(first.call_ref, second.call_ref, "two calls, two callRefs");
    assert_eq!(second.b_legs.len(), 1, "call 2 has its one outgoing leg");
    assert_ne!(
        second.b_legs[0].call_id, *looped,
        "call 2's outgoing leg has an identity of its own"
    );
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 2, "both INVITEs are accepted calls");
    assert_eq!(counts.refused_copies(), 0, "the returning INVITE is no copy");
    (first, second)
}

/// Both calls were answered and ended by a BYE.
pub fn assert_both_answered_and_ended(s: &B2buaScene, alice_call_id: &str) {
    let (first, second) = assert_two_calls(s, alice_call_id);
    for cdr in [&first, &second] {
        let k = kinds(cdr);
        assert!(k.contains(&CdrEventType::Answer) && k.contains(&CdrEventType::Bye), "{k:?}");
    }
}

/// Write `report`'s callflow under `target/seq-reports/<cell>/` and return
/// the files written.
pub fn write_callflow(report: &RunReport, cell: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports").join(cell);
    let paths = scenario_harness::report::write_all(report, &dir).expect("write the callflow");
    for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "html")) {
        eprintln!("callflow: {}", p.display());
    }
    paths
}
