//! `${leg:<id>.local-hostport}` (`PCAP2TEST_PIVOT_V3.md` §8.1) is the address the
//! leg's endpoint is bound at on the run, not the one the document states.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pivot_interpreter::{
    Booking, ClockMode, IdentityBindings, Lane, RunConfig, Sut, UriComposer, VerdictStatus,
};
use pivot_schema::PivotV3;
use scenario_harness::Harness;

struct Composer;

impl UriComposer for Composer {
    fn compose(&self, pos: &str, _form: Option<&str>, _is_ruri: bool) -> Option<String> {
        (pos == "caller").then(|| "sip:0009001@pivot.invalid".to_string())
    }

    fn frozen(&self, value: &str, _kind: Option<&str>, _is_ruri: bool) -> String {
        value.to_string()
    }
}

struct Idle;

impl Sut for Idle {
    fn active_calls(&self) -> usize {
        0
    }

    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        Err(format!("{name:?} is not an observable this lane publishes"))
    }
}

#[tokio::test(start_paused = true)]
async fn a_leg_s_local_hostport_is_its_endpoint_s_bound_address() {
    let h = Harness::new("pivot-bound-address");
    let alice = h.agent("alice", "127.0.0.1:5166").await;
    let sys = h.agent("sys", "127.0.0.1:5193").await;
    let delay = |from: &str| serde_json::json!({ "compressible": true, "from": from, "ms": 0, "timer_linked": false });
    let doc = serde_json::json!({
        "pivot_version": 3,
        "actors": [{ "endpoint": "ep-caller", "id": "uac1", "identity": "caller", "kind": "uac" }],
        "calls": [{ "attempts": [], "caller_leg": "A", "id": "c1" }],
        "case": { "family": "transparent", "id": "bound-address", "lanes": { "upstream-demo": "ok" },
                  "origin": "authored", "title": "bound address", "variant": "repro" },
        "endpoints": [{ "binding": "dedicated", "id": "ep-caller",
                        "observed": "127.0.0.1:5060", "side": "peer" }],
        "identities": [{ "forms": ["private"], "kind": "external-caller", "name": "caller" }],
        "legs": [{ "actor": "uac1", "dir": "out", "id": "A" }],
        "timing": { "expect_budget_ms": 10000, "settle_budget_ms": 10000 },
        "flow": [
            { "id": "s1", "leg": "A", "op": "send", "delay": delay("trigger"),
              "msg": { "method": "INVITE",
                       "headers": [{ "name": "X-Bound", "value": "${leg:A.local-hostport}" }],
                       "body": { "ref": "resources/uac1_offer.sdp", "rewrite": ["c=addr", "m=port"] },
                       "ruri": { "frozen": format!("sip:callee@{}", sys.addr()) },
                       "from": { "form": "private", "pos": "caller" },
                       "to": { "frozen": "sip:callee@pivot.invalid" } } },
            { "id": "s2", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s1"),
              "msg": { "cseq-method": "INVITE", "reason": "Busy Here", "status": 486 } }
        ]
    });
    let document = PivotV3::from_json(&doc.to_string()).expect("the document parses");
    let config = RunConfig::new("upstream-demo", ClockMode::Virtual, sys.addr().to_string())
        .with_identities(IdentityBindings::new().bind("caller", "private", "0009001"));
    let lane = Lane {
        agents: BTreeMap::from([("uac1".to_string(), alice.clone())]),
        route_target: sys.addr(),
        media: Booking::new("127.0.0.1", 40000),
        base_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        composer: &Composer,
        events: None,
        injector: None,
        reader: std::sync::Arc::new(sip_message::CustomParser::built()),
    };
    let dir = std::env::temp_dir().join("pivot-runs").join("bound-address");
    let system = async {
        let mut uas = sys.receive("INVITE").await;
        let bound =
            uas.request().headers().iter().find(|h| h.name == "X-Bound").map(|h| h.value.clone());
        uas.respond(486, "Busy Here").await;
        uas.expect_ack().await;
        bound
    };
    let (outcome, bound) =
        tokio::join!(pivot_interpreter::replay(document, config, lane, &Idle, &dir), system);
    let outcome = outcome.expect("the run replays and leaves its bundle");

    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert_eq!(bound.as_deref(), Some(alice.addr().to_string().as_str()));
    let _ = h.finish().await;
}
