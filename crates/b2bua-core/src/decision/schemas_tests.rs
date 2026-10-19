use call::LimiterEntry;

use super::*;

/// A route decision's `call_limiter` encodes as `[{"id":…,"limit":…}]` in
/// stated order, and that encoding decodes back to the same entries.
#[test]
fn route_call_limiter_json_is_id_then_limit_in_stated_order() {
    let r = RouteDecision {
        destination: SipDestination::new("127.0.0.1", 5060),
        new_ruri: Some("sip:127.0.0.1:5060".into()),
        new_from: None,
        new_to: None,
        update_headers: None,
        update_body: BodyUpdate::Keep,
        no_answer_ms: None,
        call_limiter: vec![
            LimiterEntry { id: "trunk-A".into(), limit: 3 },
            LimiterEntry { id: "site-B".into(), limit: 100 },
        ],
        callback_context: None,
        features: default_platform_features(),
        service_ext: Default::default(),
        subscriptions: Vec::new(),
        trace: false,
        label: None,
    };
    let encoded = serde_json::to_string(&r).unwrap();
    let pinned = r#"[{"id":"trunk-A","limit":3},{"id":"site-B","limit":100}]"#;
    assert!(encoded.contains(&format!(r#""call_limiter":{pinned}"#)), "{encoded}");
    let decoded: Vec<LimiterEntry> = serde_json::from_str(pinned).unwrap();
    assert_eq!(decoded, r.call_limiter);
}
