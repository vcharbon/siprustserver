use super::*;

fn req(to_user: &str) -> NewCallRequest {
    NewCallRequest {
        call_id: "cid".into(),
        ruri: format!("sip:{to_user}@example.com"),
        to: format!("<sip:{to_user}@example.com>"),
        ..Default::default()
    }
}

#[tokio::test]
async fn route_all_to_routes_every_call() {
    let eng = ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070);
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.destination.host, "127.0.0.1");
            assert_eq!(r.destination.port(), 5070);
            assert_eq!(r.features.platform.max_duration_sec, 3_600);
        }
        _ => panic!("expected route"),
    }
}

fn req_with_header(name: &str, value: &str) -> NewCallRequest {
    let mut r = req("bob");
    r.sip_headers.push((name.into(), value.into()));
    r
}

#[test]
fn limiter_entries_from_header_parses_array() {
    let r = req_with_header(
        "X-Api-Call",
        r#"{"action":"route","call_limiter":[{"id":"x","limit":20}]}"#,
    );
    let entries = limiter_entries_from_api_call(&r);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "x");
    assert_eq!(entries[0].limit, 20);
}

#[test]
fn limiter_entries_empty_without_header_or_field() {
    assert!(limiter_entries_from_api_call(&req("bob")).is_empty());
    // Header present but no call_limiter field → empty.
    let r = req_with_header("X-Api-Call", r#"{"action":"route"}"#);
    assert!(limiter_entries_from_api_call(&r).is_empty());
    // Malformed JSON → empty (fail-open, no limiting).
    let bad = req_with_header("X-Api-Call", "not json");
    assert!(limiter_entries_from_api_call(&bad).is_empty());
}

#[tokio::test]
async fn route_all_to_with_limiter_stress_and_header() {
    let stress = LimiterEntry { id: "global-stress".into(), limit: 999_999 };
    let eng = ScriptedDecisionEngine::route_all_to_with_limiter("127.0.0.1", 5070, Some(stress));

    // Header-less call: only the always-on stress entry.
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.call_limiter.len(), 1);
            assert_eq!(r.call_limiter[0].id, "global-stress");
        }
        _ => panic!("expected route"),
    }

    // Header-carrying call: stress entry + the header's cap entry.
    let r = req_with_header(
        "X-Api-Call",
        r#"{"action":"route","call_limiter":[{"id":"endurance-limiter","limit":20}]}"#,
    );
    match eng.new_call(r).await.unwrap() {
        NewCallResponse::Route(d) => {
            let ids: Vec<&str> = d.call_limiter.iter().map(|e| e.id.as_str()).collect();
            assert_eq!(ids, ["global-stress", "endurance-limiter"]);
            assert_eq!(d.call_limiter[1].limit, 20);
        }
        _ => panic!("expected route"),
    }
}

#[tokio::test]
async fn route_all_to_with_limiter_none_stress_is_header_only() {
    let eng = ScriptedDecisionEngine::route_all_to_with_limiter("127.0.0.1", 5070, None);
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(r) => assert!(r.call_limiter.is_empty()),
        _ => panic!("expected route"),
    }
}

#[test]
fn route_dest_from_api_call_parses_destination() {
    let r = req_with_header(
        "X-Api-Call",
        r#"{"destination":{"host":"sipp-uas-2.sipp-uas.sip-test.svc.cluster.local","port":5060}}"#,
    );
    assert_eq!(
        route_dest_from_api_call(&r),
        Some(("sipp-uas-2.sipp-uas.sip-test.svc.cluster.local".to_string(), 5060))
    );
    // Port defaults to 5060 when omitted.
    let r = req_with_header("X-Api-Call", r#"{"destination":{"host":"10.0.0.9"}}"#);
    assert_eq!(route_dest_from_api_call(&r), Some(("10.0.0.9".to_string(), 5060)));
    // Absent header / no destination / empty host / bad JSON → None (fallback).
    assert_eq!(route_dest_from_api_call(&req("bob")), None);
    assert_eq!(
        route_dest_from_api_call(&req_with_header("X-Api-Call", r#"{"action":"route"}"#)),
        None
    );
    assert_eq!(
        route_dest_from_api_call(&req_with_header("X-Api-Call", r#"{"destination":{"host":""}}"#)),
        None
    );
    assert_eq!(route_dest_from_api_call(&req_with_header("X-Api-Call", "not json")), None);
}

#[tokio::test]
async fn route_all_to_with_limiter_honors_header_destination() {
    let eng = ScriptedDecisionEngine::route_all_to_with_limiter("127.0.0.1", 5070, None);

    // Header-directed callee overrides the configured DEST for dest + R-URI.
    let r = req_with_header("X-Api-Call", r#"{"destination":{"host":"10.244.6.8","port":5060}}"#);
    match eng.new_call(r).await.unwrap() {
        NewCallResponse::Route(d) => {
            assert_eq!(d.destination.host, "10.244.6.8");
            assert_eq!(d.destination.port(), 5060);
            assert_eq!(d.new_ruri.as_deref(), Some("sip:10.244.6.8:5060"));
        }
        _ => panic!("expected route"),
    }

    // `destination.user` sets the R-URI userpart (host:port unchanged) so a
    // downstream registrar front-proxy can resolve the AOR (sip:bob@<core>).
    let r = req_with_header(
        "X-Api-Call",
        r#"{"destination":{"host":"192.0.2.1","port":25081,"user":"bob"}}"#,
    );
    match eng.new_call(r).await.unwrap() {
        NewCallResponse::Route(d) => {
            assert_eq!(d.destination.host, "192.0.2.1");
            assert_eq!(d.destination.port(), 25081);
            assert_eq!(d.new_ruri.as_deref(), Some("sip:bob@192.0.2.1:25081"));
        }
        _ => panic!("expected route"),
    }
    // Empty / absent user → userless R-URI (unchanged behaviour).
    assert_eq!(
        route_user_from_api_call(&req_with_header(
            "X-Api-Call",
            r#"{"destination":{"host":"10.0.0.9","user":""}}"#
        )),
        None
    );
    assert_eq!(
        route_user_from_api_call(&req_with_header(
            "X-Api-Call",
            r#"{"destination":{"host":"10.0.0.9"}}"#
        )),
        None
    );

    // No destination in the header → fall back to the configured DEST.
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(d) => {
            assert_eq!(d.destination.host, "127.0.0.1");
            assert_eq!(d.new_ruri.as_deref(), Some("sip:127.0.0.1:5070"));
        }
        _ => panic!("expected route"),
    }
}

#[tokio::test]
async fn route_all_to_with_limiter_walks_routes_plan_with_stress() {
    // ADR-0017 plan-reroute (plan-reroute.html) on the CLUSTER engine: a
    // `routes` list fails over per route, each carrying its own `new_ruri`
    // userpart so a register front-proxy resolves bob1 → bob2. The `stress`
    // limiter is still appended to every leg.
    let stress = LimiterEntry { id: "global-stress".into(), limit: 999_999 };
    let eng = ScriptedDecisionEngine::route_all_to_with_limiter("127.0.0.1", 5070, Some(stress));
    let plan = serde_json::json!({
        "action": "route",
        "routes": [
            {"destination": {"host": "192.0.2.1", "port": 25081}, "new_ruri": "sip:bob1@192.0.2.1:25081"},
            {"destination": {"host": "192.0.2.1", "port": 25081}, "new_ruri": "sip:bob2@192.0.2.1:25081"}
        ]
    });
    let ctx = match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.new_ruri.as_deref(), Some("sip:bob1@192.0.2.1:25081"));
            assert!(r.call_limiter.iter().any(|e| e.id == "global-stress"));
            r.callback_context.expect("remainder context")
        }
        _ => panic!("expected route #1 (bob1)"),
    };
    // b-leg 503 → walk to route #2 (bob2).
    match eng.call_failure(failure_req(Some(&ctx))).await.unwrap() {
        CallTreatment::Route(r) => {
            assert_eq!(r.new_ruri.as_deref(), Some("sip:bob2@192.0.2.1:25081"));
        }
        _ => panic!("expected route #2 (bob2)"),
    }
    // Plan-less call still falls back to DEST (no 404, no regression).
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(d) => assert_eq!(d.destination.host, "127.0.0.1"),
        _ => panic!("expected DEST fallback"),
    }
}

#[tokio::test]
async fn payload_rule_can_reject() {
    let eng = ScriptedDecisionEngine::builder()
        .on(|r| r.to.contains("blocked").then(|| reject(403, "Forbidden")))
        .fallback(|_| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
        .build();
    match eng.new_call(req("blocked")).await.unwrap() {
        NewCallResponse::Reject(rj) => assert_eq!(rj.reject_code, 403),
        _ => panic!("expected reject"),
    }
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Route(_) => {}
        _ => panic!("expected route"),
    }
}

// ── ADR-0017 numbering-plan walker ──────────────────────────────────────

fn plan_req(plan: serde_json::Value) -> NewCallRequest {
    req_with_header("X-Api-Call", &plan.to_string())
}

fn failure_req(ctx: Option<&str>) -> CallFailureRequest {
    CallFailureRequest {
        callback_context: ctx.map(str::to_string),
        failure: crate::decision::FailureInfo {
            origin: "external".into(),
            status_code: Some(503),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn plan_route_sets_all_identity_and_header_fields() {
    let eng = ScriptedDecisionEngine::numbering_plan();
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "10.0.0.9", "port": 5070},
        "new_ruri": "sip:+18001234@carrier",
        "new_from": "sip:+15551000@me",
        "new_to": "sip:+19005678@dest",
        "update_headers": {
            "P-Asserted-Identity": "sip:+15551000@me",
            "P-Access-Network-Info": "3GPP-E-UTRAN-FDD"
        }
    });
    match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.destination.host, "10.0.0.9");
            assert_eq!(r.new_ruri.as_deref(), Some("sip:+18001234@carrier"));
            assert_eq!(r.new_from.as_deref(), Some("sip:+15551000@me"));
            assert_eq!(r.new_to.as_deref(), Some("sip:+19005678@dest"));
            let h = r.update_headers.unwrap();
            assert_eq!(h.get("P-Asserted-Identity").unwrap().single(), Some("sip:+15551000@me"));
            assert_eq!(h.get("P-Access-Network-Info").unwrap().single(), Some("3GPP-E-UTRAN-FDD"));
            // No reroute / on_exhausted ⇒ no carried context.
            assert!(r.callback_context.is_none());
        }
        _ => panic!("expected route"),
    }
}

#[tokio::test]
async fn a_plan_route_carries_the_engine_force_enable_to_the_decision() {
    // This decoder is the ONLY way a plan JSON becomes a `RouteDecision`, so
    // a dropped `trace` field makes the ADR-0026 force-enable unreachable
    // from any e2e / endurance rig — the flag would read as configured and
    // do nothing. It must survive on the initial route AND on a failover one.
    let eng = ScriptedDecisionEngine::numbering_plan();
    let plan = serde_json::json!({
        "action": "route",
        "routes": [
            {"destination": {"host": "10.0.0.1", "port": 5070}},
            {"destination": {"host": "10.0.0.2", "port": 5070}, "trace": true}
        ],
        "on_exhausted": {"action": "relay"}
    });
    let ctx = match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert!(!r.trace, "a route that asks for nothing stays untraced");
            r.callback_context.expect("context carries the remainder")
        }
        _ => panic!("expected route #1"),
    };
    match eng.call_failure(failure_req(Some(&ctx))).await.unwrap() {
        CallTreatment::Route(r) => {
            assert_eq!(r.destination.host, "10.0.0.2");
            assert!(r.trace, "`trace: true` must reach the engine force-enable");
        }
        _ => panic!("expected route #2"),
    }

    // The same field on a single top-level route (no `routes` array).
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "10.0.0.9", "port": 5070},
        "trace": true
    });
    match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => assert!(r.trace),
        _ => panic!("expected route"),
    }
}

#[tokio::test]
async fn plan_reroute_walks_list_then_rejects_on_exhaustion() {
    let eng = ScriptedDecisionEngine::numbering_plan();
    let plan = serde_json::json!({
        "action": "route",
        "routes": [
            {"destination": {"host": "10.0.0.1", "port": 5070}, "new_to": "sip:+1@a"},
            {"destination": {"host": "10.0.0.2", "port": 5070}, "new_to": "sip:+1@b"}
        ],
        "on_exhausted": {"action": "reject", "code": 603, "reason": "Declined",
                         "update_headers": {"Reason": "Q.850;cause=21"}}
    });
    // First attempt → route #1, context carries the remainder.
    let ctx1 = match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.destination.host, "10.0.0.1");
            assert_eq!(r.new_to.as_deref(), Some("sip:+1@a"));
            r.callback_context.expect("context carries remainder")
        }
        _ => panic!("expected route #1"),
    };
    // First failure → route #2.
    let ctx2 = match eng.call_failure(failure_req(Some(&ctx1))).await.unwrap() {
        CallTreatment::Route(r) => {
            assert_eq!(r.destination.host, "10.0.0.2");
            assert_eq!(r.new_to.as_deref(), Some("sip:+1@b"));
            r.callback_context.expect("context carries empty remainder")
        }
        _ => panic!("expected route #2"),
    };
    // Second failure → list exhausted → the on_exhausted reject (+ Reason).
    match eng.call_failure(failure_req(Some(&ctx2))).await.unwrap() {
        CallTreatment::Reject(rj) => {
            assert_eq!(rj.reject_code, 603);
            assert_eq!(rj.reject_reason.as_deref(), Some("Declined"));
            assert_eq!(
                rj.update_headers.unwrap().get("Reason").unwrap().single(),
                Some("Q.850;cause=21")
            );
        }
        _ => panic!("expected exhaustion reject"),
    }
}

#[tokio::test]
async fn plan_route_carries_per_route_no_answer_timeout() {
    // Each plan route's `no_answer_timeout_sec` reaches the
    // RouteDecision — on the FIRST route (apply_route arms the NoAnswer
    // ring timer) AND on the failover route popped by call_failure.
    let eng = ScriptedDecisionEngine::numbering_plan();
    let plan = serde_json::json!({
        "action": "route",
        "routes": [
            {"destination": {"host": "10.0.0.1", "port": 5070}, "no_answer_timeout_sec": 2},
            {"destination": {"host": "10.0.0.2", "port": 5070}, "no_answer_timeout_sec": 3}
        ]
    });
    let ctx = match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => {
            assert_eq!(r.no_answer_timeout_sec, Some(2));
            r.callback_context.expect("remainder context")
        }
        _ => panic!("expected route #1"),
    };
    match eng.call_failure(failure_req(Some(&ctx))).await.unwrap() {
        CallTreatment::Route(r) => assert_eq!(r.no_answer_timeout_sec, Some(3)),
        _ => panic!("expected route #2"),
    }
}

#[tokio::test]
async fn plan_on_exhausted_defaults_to_relay() {
    let eng = ScriptedDecisionEngine::numbering_plan();
    let plan = serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "10.0.0.1", "port": 5070}}],
        "on_exhausted": {"action": "relay"}
    });
    let ctx = match eng.new_call(plan_req(plan)).await.unwrap() {
        NewCallResponse::Route(r) => r.callback_context.unwrap(),
        _ => panic!("expected route"),
    };
    // Only one route → next failure exhausts → Relay.
    assert!(matches!(
        eng.call_failure(failure_req(Some(&ctx))).await.unwrap(),
        CallTreatment::Relay { .. }
    ));
    // No context at all → Relay too.
    assert!(matches!(
        eng.call_failure(failure_req(None)).await.unwrap(),
        CallTreatment::Relay { .. }
    ));
}

#[tokio::test]
async fn plan_direct_reject_and_redirect() {
    let eng = ScriptedDecisionEngine::numbering_plan();
    // Direct reject with a Reason header.
    let reject_plan = serde_json::json!({
        "action": "reject", "code": 403, "reason": "Forbidden",
        "update_headers": {"Reason": "SIP;cause=403;text=\"blocked\""}
    });
    match eng.new_call(plan_req(reject_plan)).await.unwrap() {
        NewCallResponse::Reject(rj) => {
            assert_eq!(rj.reject_code, 403);
            assert!(rj.update_headers.unwrap().contains_key("Reason"));
        }
        _ => panic!("expected reject"),
    }
    // Direct 302 redirect with an ordered Contact list.
    let redirect_plan = serde_json::json!({
        "action": "redirect", "code": 302,
        "contacts": [{"uri": "sip:primary@h1", "q": 1.0}, {"uri": "sip:backup@h2", "q": 0.5}]
    });
    match eng.new_call(plan_req(redirect_plan)).await.unwrap() {
        NewCallResponse::Redirect(rd) => {
            assert_eq!(rd.code, 302);
            assert_eq!(rd.contacts.len(), 2);
            assert_eq!(rd.contacts[0].uri, "sip:primary@h1");
            assert_eq!(rd.contacts[0].q, Some(1.0));
            assert_eq!(rd.contacts[1].uri, "sip:backup@h2");
        }
        _ => panic!("expected redirect"),
    }
}

#[tokio::test]
async fn plan_absent_header_rejects_404() {
    let eng = ScriptedDecisionEngine::numbering_plan();
    match eng.new_call(req("bob")).await.unwrap() {
        NewCallResponse::Reject(rj) => assert_eq!(rj.reject_code, 404),
        _ => panic!("expected 404"),
    }
}

// An absent port is RFC 3261 §19.1.2's default; a port that
// is STATED and does not read is a refusal. Collapsing the second onto 5060
// dials the default port of a host the plan never named — the same
// fabricated-default class as an opaque routing URI.
#[test]
fn only_an_absent_port_defaults_to_5060() {
    use serde_json::json;
    assert_eq!(super::super::read_stated_port(None), Some(5060), "absent ⇒ the RFC default");
    assert_eq!(
        super::super::read_stated_port(Some(&json!(null))),
        Some(5060),
        "null ⇒ the RFC default"
    );
    assert_eq!(super::super::read_stated_port(Some(&json!(5070))), Some(5070));
    assert_eq!(super::super::read_stated_port(Some(&json!(65535))), Some(65535));
    for stated in [json!(65536), json!(0), json!(-1), json!("5070"), json!({})] {
        assert_eq!(
            super::super::read_stated_port(Some(&stated)),
            None,
            "a stated but unreadable port ({stated}) must refuse, not become 5060"
        );
    }
}

// The refusal reaches the decision: a plan route whose port does not read
// yields no route at all, rather than a route to :5060 on the same host.
#[test]
fn a_route_with_an_unreadable_port_is_not_built() {
    use serde_json::json;
    let obj = json!({"destination": {"host": "10.0.0.9", "port": 88161}});
    assert!(route_from_obj(&obj).is_none(), "an out-of-range port must refuse the route");
    let ok = json!({"destination": {"host": "10.0.0.9", "port": 5070}});
    let route = route_from_obj(&ok).expect("a readable port routes");
    assert_eq!((route.destination.host.as_str(), route.destination.port()), ("10.0.0.9", 5070));
    let portless = json!({"destination": {"host": "10.0.0.9"}});
    let route = route_from_obj(&portless).expect("a portless destination routes");
    assert_eq!(route.destination.port(), 5060, "absent port ⇒ RFC default");
}
