//! `ScriptedDecisionEngine` — a deterministic, no-HTTP decision backend that
//! emulates the jssip reference backend by inspecting the request JSON
//! (R-URI / To / `X-*` headers / body). Tests configure it with predicates so
//! the existing SIPp scenario scripts produce the same call flows; the default
//! routes every call to a configured destination with mandatory platform
//! features.

use async_trait::async_trait;

use std::collections::BTreeMap;

use super::schemas::{
    default_platform_features, BodyUpdate, CallLimiterEntry, CallTreatment, NewCallRequest,
    NewCallResponse, RedirectContact, RedirectDecision, RejectDecision, RouteDecision,
    SipDestination, SipHeaderUpdates,
};
use super::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse,
};

type NewCallRule = Box<dyn Fn(&NewCallRequest) -> Option<NewCallResponse> + Send + Sync>;
/// A scripted `/call/refer` outcome a test can request. `Hang` models an HTTP
/// request that never resolves (the sub-expiry timer is what fires).
type ReferRule = Box<dyn Fn(&CallReferRequest) -> ReferOutcome + Send + Sync>;

/// A scripted decision backend. Build with [`ScriptedDecisionEngine::route_all_to`]
/// for the common case, or [`ScriptedDecisionEngine::builder`] for payload-driven
/// rules.
pub struct ScriptedDecisionEngine {
    rules: Vec<NewCallRule>,
    fallback: NewCallFallback,
    failure: FailureRule,
    refer: ReferRule,
}

/// What the scripted adapter decides for a `/call/refer` request.
pub enum ReferOutcome {
    Allow(super::CallReferResponse),
    /// Immediate failure (the HTTP-500 case) → mapped to `error` by the router.
    Error,
    /// The HTTP request hangs forever — `call_refer` never resolves, so the
    /// re-entry never fires and the sub-expiry timer drives the outcome.
    Hang,
}

impl ScriptedDecisionEngine {
    /// The common case: route every call to `host:port` with default platform
    /// features and terminate on b-leg failure.
    pub fn route_all_to(host: impl Into<String>, port: u16) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to(&dest.0, dest.1))
            })
            .build()
    }

    /// Like [`route_all_to`](Self::route_all_to) but wires the call limiter so
    /// the endurance suite can exercise it:
    ///  - `stress` (e.g. `global-stress:999999`) is attached to EVERY call so the
    ///    full admit/release/refresh chain runs on all traffic without ever
    ///    rejecting; pass `None` to disable.
    ///  - an inbound `X-Api-Call` header carrying a `call_limiter` array is
    ///    honored and its entries appended, so a dedicated stream can enforce a
    ///    real cap (e.g. `endurance-limiter:20`). Admit is all-or-none, so a call
    ///    is rejected only when one of these entries is over its cap.
    pub fn route_all_to_with_limiter(
        host: impl Into<String>,
        port: u16,
        stress: Option<CallLimiterEntry>,
    ) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |req| {
                let mut r = route_to(&dest.0, dest.1);
                // Rewrite the b-leg Request-URI to the real callee. Without this,
                // the b-leg R-URI defaults to the a-leg's (relay.rs build_b_leg),
                // which behind an LB front proxy is the proxy's OWN VIP (the UAC
                // dialed the VIP). That is harmless when the b-leg goes pod-direct
                // (the wire destination, not the R-URI, picks the hop), but FATAL
                // when the b-leg is forced through the proxy (b2b_outbound_proxy):
                // the proxy forwards a worker-outbound request to its R-URI, so a
                // VIP R-URI bounces straight back to a worker, which re-INVITEs a
                // fresh b-leg (Max-Forwards reset to 70 each time, so never 483) →
                // an unbounded call-creation loop that OOMs the worker. The R-URI
                // MUST name the actual downstream callee.
                r.new_ruri = Some(format!("sip:{}:{}", dest.0, dest.1));
                if let Some(s) = &stress {
                    r.call_limiter.push(s.clone());
                }
                r.call_limiter.extend(limiter_entries_from_api_call(req));
                NewCallResponse::Route(r)
            })
            .build()
    }

    /// Route every call to `host:port` and authorize REFER transfers via the
    /// default `X-Api-Call`-keyed behavior (port of `mockCallReferBehavior`).
    /// This is the common REFER-scenario constructor.
    pub fn route_all_with_refer(host: impl Into<String>, port: u16) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |_req| NewCallResponse::Route(route_to(&dest.0, dest.1)))
            .on_refer(default_call_refer)
            .build()
    }

    /// A backend that treats the inbound `X-Api-Call` header as a full
    /// **call-treatment plan** (ADR-0017): route + identity/header rewrites, an
    /// ordered reroute list walked on b-leg failure, and direct reject / 302
    /// redirect. The remaining reroute list rides `callback_context` between hops
    /// (the platform keeps it opaque; a real HTTP backend would hold it itself).
    /// Absent / non-plan `X-Api-Call` → reject 404. This is how a test (Alice)
    /// injects an advanced numbering plan.
    pub fn numbering_plan() -> Self {
        Self::builder()
            .fallback(new_call_from_plan)
            .on_failure(failure_from_context)
            .build()
    }

    pub fn builder() -> ScriptedBuilder {
        ScriptedBuilder {
            rules: Vec::new(),
            fallback: None,
            failure: None,
            refer: None,
        }
    }
}

// ── ADR-0017 numbering-plan walker ──────────────────────────────────────────

/// Parse the inbound `X-Api-Call` header as a JSON plan object.
fn parse_api_call_plan(req: &NewCallRequest) -> Option<serde_json::Value> {
    let raw = req.sip_headers.get("X-Api-Call")?;
    serde_json::from_str(raw).ok()
}

/// `{name: value | null}` → [`SipHeaderUpdates`] (`null` = remove).
fn parse_update_headers(v: Option<&serde_json::Value>) -> Option<SipHeaderUpdates> {
    v.and_then(|x| x.as_object()).map(|m| {
        let mut out = SipHeaderUpdates::new();
        for (k, val) in m {
            out.insert(k.clone(), val.as_str().map(str::to_string));
        }
        out
    })
}

/// `[{uri, q?}]` → ordered redirect Contact list.
fn parse_contacts(v: Option<&serde_json::Value>) -> Vec<RedirectContact> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(RedirectContact {
                        uri: c.get("uri")?.as_str()?.to_string(),
                        q: c.get("q").and_then(|q| q.as_f64()).map(|q| q as f32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a [`RouteDecision`] from one plan route object (destination + identity
/// rewrites + header rewrites + optional per-route limiter). `callback_context`
/// is filled in by the caller with the remaining plan.
fn route_from_obj(obj: &serde_json::Value) -> Option<RouteDecision> {
    let dest = obj.get("destination")?;
    let host = dest.get("host")?.as_str()?.to_string();
    let port = dest.get("port").and_then(|p| p.as_u64()).map(|p| p as u16).unwrap_or(5060);
    let mut r = route_to(&host, port);
    r.new_ruri = obj.get("new_ruri").and_then(|v| v.as_str()).map(str::to_string);
    r.new_from = obj.get("new_from").and_then(|v| v.as_str()).map(str::to_string);
    r.new_to = obj.get("new_to").and_then(|v| v.as_str()).map(str::to_string);
    r.update_headers = parse_update_headers(obj.get("update_headers"));
    if let Some(arr) = obj.get("call_limiter").and_then(|v| v.as_array()) {
        for e in arr {
            if let (Some(id), Some(limit)) = (
                e.get("id").and_then(|v| v.as_str()),
                e.get("limit").and_then(|v| v.as_i64()),
            ) {
                r.call_limiter.push(CallLimiterEntry { id: id.to_string(), limit });
            }
        }
    }
    Some(r)
}

/// Map a plan treatment object (`action`: route / reject / redirect / relay) to a
/// [`CallTreatment`]. A route with no resolvable destination falls back to `None`.
fn treatment_from_obj(obj: &serde_json::Value) -> Option<CallTreatment> {
    match obj.get("action").and_then(|v| v.as_str()).unwrap_or("route") {
        "reject" => Some(CallTreatment::Reject(RejectDecision {
            reject_code: obj.get("code").and_then(|v| v.as_u64()).unwrap_or(603) as u16,
            reject_reason: obj.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            update_headers: parse_update_headers(obj.get("update_headers")),
        })),
        "redirect" => Some(CallTreatment::Redirect(RedirectDecision {
            code: obj.get("code").and_then(|v| v.as_u64()).unwrap_or(302) as u16,
            reason: obj.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            contacts: parse_contacts(obj.get("contacts")),
            update_headers: parse_update_headers(obj.get("update_headers")),
        })),
        "relay" => Some(CallTreatment::Relay),
        _ => route_from_obj(obj).map(CallTreatment::Route),
    }
}

/// Serialize the *remaining* reroute list + the `on_exhausted` treatment — the
/// opaque blob the platform round-trips as `callback_context` (ADR-0017 X3).
fn remainder_context(rest: &[serde_json::Value], on_exhausted: &serde_json::Value) -> String {
    serde_json::json!({ "routes": rest, "on_exhausted": on_exhausted }).to_string()
}

/// `new_call`: parse the plan, emit the first treatment, and stash the remainder
/// in `callback_context`.
fn new_call_from_plan(req: &NewCallRequest) -> NewCallResponse {
    let plan = match parse_api_call_plan(req) {
        Some(p) => p,
        None => return reject(404, "Not Found"),
    };
    // Direct reject / redirect at the top level.
    match plan.get("action").and_then(|v| v.as_str()) {
        Some("reject") | Some("redirect") => {
            return treatment_from_obj(&plan).unwrap_or_else(|| reject(603, "Declined"));
        }
        _ => {}
    }
    // Route (+ reroute): the ordered attempt list is `routes`, or the top-level
    // object treated as a single route.
    let routes: Vec<serde_json::Value> = plan
        .get("routes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| vec![plan.clone()]);
    let first = match routes.first().and_then(route_from_obj) {
        Some(r) => r,
        None => return reject(404, "Not Found"),
    };
    let on_exhausted = plan
        .get("on_exhausted")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "action": "relay" }));
    // Only carry a context when there is something to fail over to (a remaining
    // route or an explicit exhaustion treatment) — a plain single route keeps the
    // pre-failover behaviour (relay + terminate on failure).
    let mut r = first;
    if routes.len() > 1 || plan.get("on_exhausted").is_some() {
        r.callback_context = Some(remainder_context(&routes[1..], &on_exhausted));
    }
    NewCallResponse::Route(r)
}

/// `call_failure`: pop the next route off `callback_context`, re-stash the tail;
/// on an empty list return the plan's `on_exhausted` treatment (ADR-0017 X3).
fn failure_from_context(req: &CallFailureRequest) -> CallFailureResponse {
    let ctx = match req
        .callback_context
        .as_ref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    {
        Some(v) => v,
        None => return CallTreatment::Relay,
    };
    let routes = ctx.get("routes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let on_exhausted = ctx
        .get("on_exhausted")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "action": "relay" }));
    if let Some(next) = routes.first() {
        if let Some(mut r) = route_from_obj(next) {
            r.callback_context = Some(remainder_context(&routes[1..], &on_exhausted));
            return CallTreatment::Route(r);
        }
    }
    treatment_from_obj(&on_exhausted).unwrap_or(CallTreatment::Relay)
}

/// Default scripted `/call/refer` behavior, keyed on the REFER's `X-Api-Call`
/// JSON header (port of `mockCallReferBehavior`, MockServer.ts:192-244):
///   - `refer-reject-403` → reject 403/Forbidden (or payload code/reason)
///   - `refer-http-500`   → `Error` (→ router maps to outcome `error`/500)
///   - `refer-http-timeout` → `Hang` (HTTP never resolves; sub-expiry fires)
///   - `refer-allow-c`    → allow to `destination` (default 127.0.0.1:5667)
///   - default / missing  → reject 603/Declined
pub fn default_call_refer(req: &CallReferRequest) -> ReferOutcome {
    let raw = match req.sip_headers.get("X-Api-Call") {
        Some(v) => v,
        None => {
            return ReferOutcome::Allow(CallReferResponse::Reject {
                code: 603,
                reason: Some("Declined".into()),
            })
        }
    };
    let instruction: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return ReferOutcome::Allow(CallReferResponse::Reject {
                code: 603,
                reason: Some("Declined".into()),
            })
        }
    };
    let key = instruction.get("refer_key").and_then(|v| v.as_str()).unwrap_or("");
    match key {
        "refer-reject-403" => ReferOutcome::Allow(CallReferResponse::Reject {
            code: instruction.get("reject_code").and_then(|v| v.as_u64()).map(|c| c as u16).unwrap_or(403),
            reason: Some(
                instruction
                    .get("reject_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Forbidden")
                    .to_string(),
            ),
        }),
        "refer-http-500" => ReferOutcome::Error,
        "refer-http-timeout" => ReferOutcome::Hang,
        "refer-allow-c" => {
            let dest = instruction.get("destination");
            let host = dest
                .and_then(|d| d.get("host"))
                .and_then(|v| v.as_str())
                .unwrap_or("127.0.0.1")
                .to_string();
            let port = dest
                .and_then(|d| d.get("port"))
                .and_then(|v| v.as_u64())
                .map(|p| p as u16)
                .unwrap_or(5667);
            let new_refer_to = instruction.get("new_refer_to").and_then(|v| v.as_str()).map(str::to_string);
            let no_answer_timeout_sec = instruction
                .get("no_answer_timeout_sec")
                .and_then(|v| v.as_i64());
            let callback_context = instruction.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
            let update_headers = instruction.get("update_headers").and_then(|v| v.as_object()).map(|m| {
                let mut out: super::schemas::SipHeaderUpdates = BTreeMap::new();
                for (k, val) in m {
                    out.insert(k.clone(), val.as_str().map(str::to_string));
                }
                out
            });
            ReferOutcome::Allow(CallReferResponse::Allow {
                destination: SipDestination::new(host, port),
                new_refer_to,
                update_headers,
                no_answer_timeout_sec,
                callback_context,
            })
        }
        _ => ReferOutcome::Allow(CallReferResponse::Reject {
            code: 603,
            reason: Some("Declined".into()),
        }),
    }
}

/// Parse an inbound `X-Api-Call` JSON header into call-limiter admission
/// entries — `{"...","call_limiter":[{"id":"x","limit":20}]}`. Absent header,
/// non-JSON, or a missing/!array `call_limiter` field all yield an empty vec
/// (no limiting). Entries missing `id`/`limit` are skipped.
pub fn limiter_entries_from_api_call(req: &NewCallRequest) -> Vec<CallLimiterEntry> {
    let raw = match req.sip_headers.get("X-Api-Call") {
        Some(v) => v,
        None => return Vec::new(),
    };
    let instruction: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    instruction
        .get("call_limiter")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(CallLimiterEntry {
                        id: e.get("id")?.as_str()?.to_string(),
                        limit: e.get("limit")?.as_i64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a [`RouteDecision`] to `host:port` with default platform features.
pub fn route_to(host: &str, port: u16) -> RouteDecision {
    RouteDecision {
        destination: SipDestination::new(host, port),
        new_ruri: None,
        new_from: None,
        new_to: None,
        update_headers: None,
        update_body: BodyUpdate::Keep,
        no_answer_timeout_sec: None,
        call_limiter: Vec::new(),
        callback_context: None,
        features: default_platform_features(),
        service_ext: Default::default(),
    }
}

/// Build a [`RouteDecision`] to `host:port` with the `relayFirst18xTo180`
/// feature active under `strategy` (the scripted equivalent of the wire
/// `relay_first_18x_to_180` field; `true`→`DropSdp` is the suppress mode).
pub fn route_to_with_18x(
    host: &str,
    port: u16,
    strategy: call::features::RelayFirst18xStrategy,
) -> RouteDecision {
    let mut r = route_to(host, port);
    r.features.relay_first_18x_to_180 =
        Some(call::features::RelayFirst18xTo180Feature { strategy });
    r
}

/// A reject decision.
pub fn reject(code: u16, reason: impl Into<String>) -> NewCallResponse {
    NewCallResponse::Reject(RejectDecision {
        reject_code: code,
        reject_reason: Some(reason.into()),
        update_headers: None,
    })
}

type NewCallFallback = Box<dyn Fn(&NewCallRequest) -> NewCallResponse + Send + Sync>;
type FailureRule = Box<dyn Fn(&CallFailureRequest) -> CallFailureResponse + Send + Sync>;

pub struct ScriptedBuilder {
    rules: Vec<NewCallRule>,
    fallback: Option<NewCallFallback>,
    failure: Option<FailureRule>,
    refer: Option<ReferRule>,
}

impl ScriptedBuilder {
    /// Add a payload predicate → response rule (first match wins).
    pub fn on(
        mut self,
        rule: impl Fn(&NewCallRequest) -> Option<NewCallResponse> + Send + Sync + 'static,
    ) -> Self {
        self.rules.push(Box::new(rule));
        self
    }

    pub fn fallback(
        mut self,
        f: impl Fn(&NewCallRequest) -> NewCallResponse + Send + Sync + 'static,
    ) -> Self {
        self.fallback = Some(Box::new(f));
        self
    }

    pub fn on_failure(
        mut self,
        f: impl Fn(&CallFailureRequest) -> CallFailureResponse + Send + Sync + 'static,
    ) -> Self {
        self.failure = Some(Box::new(f));
        self
    }

    /// Script the `/call/refer` decision (REFER transfer authorization).
    pub fn on_refer(
        mut self,
        f: impl Fn(&CallReferRequest) -> ReferOutcome + Send + Sync + 'static,
    ) -> Self {
        self.refer = Some(Box::new(f));
        self
    }

    pub fn build(self) -> ScriptedDecisionEngine {
        ScriptedDecisionEngine {
            rules: self.rules,
            fallback: self
                .fallback
                .unwrap_or_else(|| Box::new(|_| reject(404, "Not Found"))),
            failure: self.failure.unwrap_or_else(|| Box::new(|_| CallTreatment::Relay)),
            refer: self.refer.unwrap_or_else(|| {
                // Default: transfer is dormant — reject 501 (the pre-slice stub).
                Box::new(|_| {
                    ReferOutcome::Allow(CallReferResponse::Reject {
                        code: 501,
                        reason: Some("Not Implemented".into()),
                    })
                })
            }),
        }
    }
}

#[async_trait]
impl CallDecisionEngine for ScriptedDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        for rule in &self.rules {
            if let Some(resp) = rule(&req) {
                return Ok(resp);
            }
        }
        Ok((self.fallback)(&req))
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Ok((self.failure)(&req))
    }

    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        match (self.refer)(&req) {
            ReferOutcome::Allow(resp) => Ok(resp),
            ReferOutcome::Error => {
                Err(CallDecisionError::Unavailable("scripted /call/refer error".into()))
            }
            // The HTTP request hangs: never resolve. The detached interpreter
            // task awaits this forever (it is dropped at process exit); the
            // sub-expiry timer fires the outcome instead.
            ReferOutcome::Hang => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
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
        r.sip_headers.insert(name.into(), value.into());
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
        let stress = CallLimiterEntry { id: "global-stress".into(), limit: 999_999 };
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
                limiter_id: None,
            },
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
                assert_eq!(h.get("P-Asserted-Identity").unwrap().as_deref(), Some("sip:+15551000@me"));
                assert_eq!(h.get("P-Access-Network-Info").unwrap().as_deref(), Some("3GPP-E-UTRAN-FDD"));
                // No reroute / on_exhausted ⇒ no carried context.
                assert!(r.callback_context.is_none());
            }
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
                    rj.update_headers.unwrap().get("Reason").unwrap().as_deref(),
                    Some("Q.850;cause=21")
                );
            }
            _ => panic!("expected exhaustion reject"),
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
            CallTreatment::Relay
        ));
        // No context at all → Relay too.
        assert!(matches!(
            eng.call_failure(failure_req(None)).await.unwrap(),
            CallTreatment::Relay
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
}
