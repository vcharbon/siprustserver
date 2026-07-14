//! JSON marshalling for the fire-and-forget decision callouts: the shared
//! route-result payload builder and the request parsers that rebuild typed
//! decision requests from a seed rule's JSON.

/// Serialize a [`RouteDecision`](crate::decision::RouteDecision) into the
/// internal-event payload the route-fold rules (`failover-create-leg` /
/// `release-reroute`) consume: destination, identity/header rewrites, the
/// output-parity fields the initial `apply_route` honors (features,
/// service_ext, update_body, subscriptions), and any limiter holds the
/// dispatching task **already admitted** (`(entries, window)`). ONE builder
/// shared by the failover and release fire-and-forget arms, so the two folds
/// can never drift.
pub(super) fn route_result_payload(
    route: crate::decision::RouteDecision,
    admitted: Option<(Vec<(String, i64)>, i64)>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut p = serde_json::Map::new();
    p.insert(
        "destination".into(),
        serde_json::json!({
            "host": route.destination.host,
            "port": route.destination.port,
        }),
    );
    if let Some(v) = route.new_ruri {
        p.insert("new_ruri".into(), serde_json::json!(v));
    }
    if let Some(v) = route.new_from {
        p.insert("new_from".into(), serde_json::json!(v));
    }
    if let Some(v) = route.new_to {
        p.insert("new_to".into(), serde_json::json!(v));
    }
    if let Some(v) = route.update_headers {
        p.insert("update_headers".into(), serde_json::json!(v));
    }
    if let Some(v) = route
        .no_answer_timeout_sec
        .or(route.features.no_answer_timeout_sec)
    {
        p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
    }
    if let Some(v) = route.callback_context {
        p.insert("callback_context".into(), serde_json::json!(v));
    }
    // Route parity: the fields the initial `apply_route` honors, forwarded to
    // the resolution rule.
    if let Ok(v) = serde_json::to_value(&route.features) {
        p.insert("features".into(), v);
    }
    if !route.service_ext.is_empty() {
        p.insert(
            "service_ext".into(),
            serde_json::Value::Object(route.service_ext.into_iter().collect()),
        );
    }
    // Always present (even when empty): the latest applied route OWNS the
    // subscription registry, so a route with no `subscribe[]` must CLEAR a
    // previous route's — exactly what `apply_route` does on the initial path.
    if let Ok(v) = serde_json::to_value(&route.subscriptions) {
        p.insert("subscriptions".into(), v);
    }
    match route.update_body {
        crate::decision::BodyUpdate::Keep => {}
        crate::decision::BodyUpdate::Drop => {
            p.insert("update_body".into(), serde_json::Value::Null);
        }
        crate::decision::BodyUpdate::Replace(s) => {
            p.insert("update_body".into(), serde_json::json!(s));
        }
    }
    if let Some((entries, window)) = admitted {
        let entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, limit)| serde_json::json!({"id": id, "limit": limit}))
            .collect();
        p.insert(
            "call_limiter".into(),
            serde_json::json!({"window": window, "entries": entries}),
        );
    }
    p
}

/// Rebuild a [`CallReferRequest`](crate::decision::CallReferRequest) from the
/// JSON the seed rule emitted.
pub(super) fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let sip_headers = v
        .get("sip_headers")
        .and_then(|x| x.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers,
        snapshot: crate::decision::CallSnapshot::default(),
    }
}

/// Rebuild a [`CallReleaseRequest`](crate::decision::CallReleaseRequest) from
/// the JSON the `max-duration` seed rule emitted, attaching the call-scoped
/// snapshot the dispatch site built from the authoritative call.
pub(super) fn parse_call_release_request(
    v: &serde_json::Value,
    snapshot: crate::decision::CallSnapshot,
) -> crate::decision::CallReleaseRequest {
    // The closed v1 set is the single `max_call_duration`; parse it via serde
    // (the enum's snake_case wire form) and default to it — the one event that
    // can currently emit this seed.
    let event = v
        .get("event")
        .and_then(|x| serde_json::from_value::<call::ReleaseEventKind>(x.clone()).ok())
        .unwrap_or(call::ReleaseEventKind::MaxCallDuration);
    crate::decision::CallReleaseRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        event,
        snapshot,
    }
}

/// Rebuild a [`CallFailureRequest`](crate::decision::CallFailureRequest) from
/// the JSON the seed rule emitted. The call-scoped `snapshot` is not part of
/// the rule JSON — the dispatch site attaches it from the authoritative call.
pub(super) fn parse_call_failure_request(v: &serde_json::Value) -> crate::decision::CallFailureRequest {
    crate::decision::CallFailureRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        failure: crate::decision::FailureInfo {
            origin: v
                .get("origin")
                .and_then(|x| x.as_str())
                .unwrap_or("external")
                .to_string(),
            status_code: v
                .get("sip_code")
                .and_then(|x| x.as_u64())
                .map(|c| c as u16),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
            failed_leg_id: v
                .get("failed_leg_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // `[[name, value], …]` — wire order and duplicates preserved.
            sip_headers: v
                .get("sip_headers")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|pair| {
                            let p = pair.as_array()?;
                            Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        snapshot: crate::decision::CallSnapshot::default(),
    }
}
