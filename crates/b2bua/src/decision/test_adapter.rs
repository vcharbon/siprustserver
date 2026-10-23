//! `ScriptedDecisionEngine` — a deterministic, no-HTTP decision backend that
//! emulates the jssip reference backend by inspecting the request JSON
//! (R-URI / To / `X-*` headers / body). Tests configure it with predicates so
//! the existing SIPp scenario scripts produce the same call flows; the default
//! routes every call to a configured destination with mandatory platform
//! features.

use std::collections::BTreeMap;

use async_trait::async_trait;
use call::LimiterEntry;

use super::SipHeaderUpdates;
use super::{
    default_platform_features, BodyUpdate, CallTreatment, NewCallRequest, NewCallResponse,
    RedirectContact, RedirectDecision, RejectDecision, RouteDecision, SipDestination,
};
use super::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
};

type NewCallRule = Box<dyn Fn(&NewCallRequest) -> Option<NewCallResponse> + Send + Sync>;
/// A scripted `/call/refer` outcome a test can request. `Hang` models an HTTP
/// request that never resolves (the sub-expiry timer is what fires).
type ReferRule = Box<dyn Fn(&CallReferRequest) -> ReferOutcome + Send + Sync>;
/// A scripted `call_release` outcome.
type ReleaseRule = Box<dyn Fn(&CallReleaseRequest) -> ReleaseOutcome + Send + Sync>;

/// A scripted decision backend. Build with [`ScriptedDecisionEngine::route_all_to`]
/// for the common case, or [`ScriptedDecisionEngine::builder`] for payload-driven
/// rules.
pub struct ScriptedDecisionEngine {
    rules: Vec<NewCallRule>,
    fallback: NewCallFallback,
    failure: FailureRule,
    refer: ReferRule,
    release: Option<ReleaseRule>,
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

/// What the scripted adapter decides for a `call_release` consult.
pub enum ReleaseOutcome {
    /// A decided response (`Release` or a reroute `Route`).
    Respond(CallReleaseResponse),
    /// Immediate backend failure → the dispatch falls back to local teardown.
    Error,
    /// The HTTP request hangs forever — the `DeadlineDecisionEngine` bound is
    /// what resolves it (→ `Unavailable` → local teardown).
    Hang,
}

impl ScriptedDecisionEngine {
    /// The common case: route every call to `host:port` with default platform
    /// features and terminate on b-leg failure.
    pub fn route_all_to(host: impl Into<String>, port: u16) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |req| {
                let mut r = route_to(&dest.0, dest.1);
                r.new_ruri = Some(routed_ruri(req, &dest.0, dest.1));
                NewCallResponse::Route(r)
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
        stress: Option<LimiterEntry>,
    ) -> Self {
        // Every route states the REFER arm: this engine answers `/call/refer`
        // (`.on_refer` below), i.e. it stands for a deployment whose backend
        // processes transfers itself.
        Self::api_call_engine(host, port, stress, true)
    }

    /// [`route_all_to_with_limiter`](Self::route_all_to_with_limiter) whose
    /// routes state NO feature of their own: a call is processed locally for
    /// REFER only when its own `X-Api-Call` says `features.refer`. The REPLAY
    /// lane's engine — a replayed document must state everything it wants the
    /// platform to do, so nothing may be implied by the backend's identity.
    pub fn route_by_api_call(host: impl Into<String>, port: u16) -> Self {
        Self::api_call_engine(host, port, None, false)
    }

    fn api_call_engine(
        host: impl Into<String>,
        port: u16,
        stress: Option<LimiterEntry>,
        refer_by_default: bool,
    ) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |req| {
                // ADR-0017 plan-reroute (the plan-reroute.html model): when the
                // caller supplies a multi-route `routes` list, walk it — the first
                // route now, the remainder failed-over to by `failure_from_context`
                // (wired via `.on_failure` below). Each route carries its own
                // `destination` + identity/`new_ruri`, so a register front-proxy can
                // resolve a different AOR per attempt (bob1 503 → bob2). The single-
                // destination + DEST-fallback path below serves plan-less
                // and `{destination}`-only traffic (the sipp/endurance streams).
                if api_call_has_routes(req) {
                    let mut resp = new_call_from_plan(req);
                    if let NewCallResponse::Route(r) = &mut resp {
                        if let Some(s) = &stress {
                            r.call_limiter.push(s.clone());
                        }
                        r.call_limiter.extend(limiter_entries_from_api_call(req));
                        if refer_by_default {
                            r.features.refer.get_or_insert_with(Default::default);
                        }
                    }
                    return resp;
                }
                // The UAC may pin the b-leg callee via `X-Api-Call.destination`
                // (e.g. a specific `sipp-uas` pod, addressed by its stable headless
                // DNS name); absent that, fall back to the configured `B2BUA_DEST`.
                // This is the test control channel that lets the UAC distribute
                // calls across the callee pods itself, so Bob is reached pod-direct
                // from the LB VIP with NO kube-proxy ClusterIP NAT. A DNS name here
                // is resolved to an IP downstream in `apply_route` (the proxy and
                // transport only accept numeric next-hops).
                let (dh, dp) =
                    route_dest_from_api_call(req).unwrap_or_else(|| (dest.0.clone(), dest.1));
                let mut r = route_to(&dh, dp);
                // Rewrite the b-leg Request-URI to the real callee. Without this,
                // the b-leg R-URI defaults to the a-leg's (relay.rs build_b_leg),
                // which behind an LB front proxy is the proxy's OWN VIP (the UAC
                // dialed the VIP). That is harmless when the b-leg goes pod-direct
                // (the wire destination, not the R-URI, picks the hop), but FATAL
                // when the b-leg is forced through the proxy (b2b_outbound_proxy):
                // the proxy forwards a worker-outbound request to its R-URI, so a
                // VIP R-URI bounces straight back to a worker, which re-INVITEs a
                // fresh b-leg — a call-creation storm the §16.6 hop gate ends
                // only after the inherited budget is spent. The R-URI MUST name
                // the actual downstream callee.
                //
                // The host:port above is non-negotiable (the anti-loop invariant),
                // but the caller MAY set the R-URI *userpart* via
                // `X-Api-Call.destination.user` so a downstream registrar front-
                // proxy can resolve the AOR from the R-URI (e.g. `sip:bob@<core>`
                // → registrar lookup "bob"). Userpart only — host:port stays the
                // validated wire destination — so the invariant still holds.
                r.new_ruri = Some(match route_user_from_api_call(req) {
                    Some(user) => format!("sip:{user}@{dh}:{dp}"),
                    None => format!("sip:{dh}:{dp}"),
                });
                if let Some(s) = &stress {
                    r.call_limiter.push(s.clone());
                }
                r.call_limiter.extend(limiter_entries_from_api_call(req));
                r.features.refer = refer_feature_from_api_call(req);
                if refer_by_default {
                    r.features.refer.get_or_insert_with(Default::default);
                }
                NewCallResponse::Route(r)
            })
            // Reroute/failover walker (no-op unless a `routes` plan set a
            // callback_context above; otherwise returns Relay).
            .on_failure(failure_from_context)
            // REFER blind-transfer authorization (the deployed cluster engine).
            // Keyed on the REFER's `X-Api-Call` (`refer_key` + `destination`),
            // mirroring the fake infra's `/call/refer` wiring. Without this the
            // builder default rejects every REFER 501, so the worker accepts the
            // REFER (202 + NOTIFY) but never dials the transfer C leg — the
            // `transfer-refer-media` real cell hangs at `bob2.receive(INVITE)`.
            // Inert for non-REFER traffic (the endurance/sipp streams never REFER).
            .on_refer(default_call_refer)
            .build()
    }

    /// Route every call to `host:port` with LOCAL REFER processing activated,
    /// and authorize the transfers via the default `X-Api-Call`-keyed behavior.
    /// This is the common REFER-scenario constructor: the two halves belong
    /// together — a backend that answers `/call/refer` is one whose routes state
    /// `features.refer`.
    pub fn route_all_with_refer(host: impl Into<String>, port: u16) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to_processing_refer(&dest.0, dest.1))
            })
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
        Self::builder().fallback(new_call_from_plan).on_failure(failure_from_context).build()
    }

    pub fn builder() -> ScriptedBuilder {
        ScriptedBuilder {
            rules: Vec::new(),
            fallback: None,
            failure: None,
            refer: None,
            release: None,
        }
    }
}

// ── ADR-0017 numbering-plan walker ──────────────────────────────────────────

fn parse_api_call_plan(req: &NewCallRequest) -> Option<serde_json::Value> {
    let raw = req.sip_header("X-Api-Call")?;
    serde_json::from_str(raw).ok()
}

/// True when `X-Api-Call` carries a multi-route plan (a `routes` array) — the
/// ADR-0017 reroute/failover shape, as opposed to a single `{destination}`.
fn api_call_has_routes(req: &NewCallRequest) -> bool {
    parse_api_call_plan(req).and_then(|v| v.get("routes").map(|r| r.is_array())).unwrap_or(false)
}

/// `{name: line | [lines] | null}` → [`SipHeaderUpdates`] (`null` = remove);
/// absent or not an object = no statement.
fn parse_update_headers(v: Option<&serde_json::Value>) -> Option<SipHeaderUpdates> {
    v.filter(|x| x.is_object()).and_then(|x| serde_json::from_value(x.clone()).ok())
}

/// The optional `label` of a plan object: absent (or not a string) = none.
fn parse_label(obj: &serde_json::Value) -> Option<String> {
    obj.get("label").and_then(|v| v.as_str()).map(str::to_string)
}

/// A plan object's `service_ext` map: absent = empty.
fn parse_service_ext(obj: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    obj.get("service_ext")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// `[{uri, q?}]` → ordered redirect Contact list.
fn parse_contacts(v: Option<&serde_json::Value>) -> Vec<RedirectContact> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(RedirectContact {
                        uri: c.get("uri")?.as_str()?.to_string(),
                        q: c.get("q")
                            .filter(|q| q.is_number())
                            .and_then(|q| q.to_string().parse().ok()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A plan's status code or port, when it is a `u16`.
fn status_code(v: &serde_json::Value) -> Option<u16> {
    v.as_u64().and_then(|n| u16::try_from(n).ok())
}

/// Build a [`RouteDecision`] from one plan route object (destination + identity
/// rewrites + header rewrites + optional per-route limiter). `callback_context`
/// is filled in by the caller with the remaining plan.
fn route_from_obj(obj: &serde_json::Value) -> Option<RouteDecision> {
    let dest = obj.get("destination")?;
    let host = dest.get("host")?.as_str()?.to_string();
    let port = super::read_stated_port(dest.get("port"))?;
    let mut r = route_to(&host, port);
    // A plan route that states its own Request-URI wins; one that states none
    // keeps the routed-target default rather than falling back to the a-leg's.
    if let Some(text) = obj.get("new_ruri").and_then(|v| v.as_str()) {
        r.new_ruri = Some(text.to_string());
    }
    r.new_from = obj.get("new_from").and_then(|v| v.as_str()).map(str::to_string);
    r.new_to = obj.get("new_to").and_then(|v| v.as_str()).map(str::to_string);
    // Per-route ring deadline: the dialed b-leg arms `TimerType::NoAnswer` at
    // its first 18x, so a ring-forever hop advances the plan (the `no-answer`
    // rule POSTs /call/failure) like a reject would.
    r.no_answer_ms = obj.get("no_answer_ms").and_then(|v| v.as_i64());
    r.update_headers = parse_update_headers(obj.get("update_headers"));
    // The route's stated headers by scope (`StatedHeaders`); absent or
    // unreadable states none.
    r.features.stated_headers =
        obj.get("stated_headers").and_then(|v| serde_json::from_value(v.clone()).ok());
    // The REFER arm: a plan states per call whether the platform processes a
    // transfer itself or relays the REFER on.
    r.features.refer = refer_feature_from_obj(obj);
    // The engine force-enable (ADR-0026 §3). Read here or an e2e/endurance rig
    // could never reach it: this is the only JSON → `RouteDecision` decoder, so
    // dropping the field makes `"trace": true` in an `X-Api-Call` plan a no-op.
    r.trace = obj.get("trace").and_then(|v| v.as_bool()).unwrap_or(false);
    r.label = parse_label(obj);
    if let Some(arr) = obj.get("call_limiter").and_then(|v| v.as_array()) {
        for e in arr {
            if let (Some(id), Some(limit)) =
                (e.get("id").and_then(|v| v.as_str()), e.get("limit").and_then(|v| v.as_i64()))
            {
                r.call_limiter.push(LimiterEntry { id: id.to_string(), limit });
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
            reject_code: obj.get("code").and_then(status_code).unwrap_or(603),
            reject_reason: obj.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            update_headers: parse_update_headers(obj.get("update_headers")),
            service_ext: parse_service_ext(obj),
            label: parse_label(obj),
        })),
        "redirect" => Some(CallTreatment::Redirect(RedirectDecision {
            code: obj.get("code").and_then(status_code).unwrap_or(302),
            reason: obj.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            contacts: parse_contacts(obj.get("contacts")),
            update_headers: parse_update_headers(obj.get("update_headers")),
            service_ext: parse_service_ext(obj),
            label: parse_label(obj),
        })),
        "relay" => Some(CallTreatment::Relay { label: parse_label(obj) }),
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
    // route or an explicit exhaustion treatment) — a plain single route relays
    // and terminates on failure.
    let mut r = first;
    if routes.len() > 1 || plan.get("on_exhausted").is_some() {
        r.callback_context = Some(remainder_context(routes.get(1..).unwrap_or(&[]), &on_exhausted));
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
        None => return CallTreatment::Relay { label: None },
    };
    let routes = ctx.get("routes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let on_exhausted = ctx
        .get("on_exhausted")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "action": "relay" }));
    if let Some(next) = routes.first() {
        if let Some(mut r) = route_from_obj(next) {
            let rest = routes.get(1..).unwrap_or(&[]);
            r.callback_context = Some(remainder_context(rest, &on_exhausted));
            return CallTreatment::Route(r);
        }
    }
    treatment_from_obj(&on_exhausted).unwrap_or(CallTreatment::Relay { label: None })
}

/// Default scripted `/call/refer` behavior, keyed on the REFER's `X-Api-Call`
/// JSON header:
///   - `refer-reject-403` → reject 403/Forbidden (or payload code/reason)
///   - `refer-http-500`   → `Error` (→ router maps to outcome `error`/500)
///   - `refer-http-timeout` → `Hang` (HTTP never resolves; sub-expiry fires)
///   - `refer-allow-c`    → allow to `destination` (default 127.0.0.1:5667)
///   - default / missing  → reject 603/Declined
pub fn default_call_refer(req: &CallReferRequest) -> ReferOutcome {
    let raw = match req.sip_header("X-Api-Call") {
        Some(v) => v,
        None => {
            return ReferOutcome::Allow(CallReferResponse::Reject {
                code: 603,
                reason: Some("Declined".into()),
                label: None,
            })
        }
    };
    let instruction: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return ReferOutcome::Allow(CallReferResponse::Reject {
                code: 603,
                reason: Some("Declined".into()),
                label: None,
            })
        }
    };
    let key = instruction.get("refer_key").and_then(|v| v.as_str()).unwrap_or("");
    match key {
        "refer-reject-403" => ReferOutcome::Allow(CallReferResponse::Reject {
            code: instruction.get("reject_code").and_then(status_code).unwrap_or(403),
            reason: Some(
                instruction
                    .get("reject_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Forbidden")
                    .to_string(),
            ),
            label: parse_label(&instruction),
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
            let port = dest.and_then(|d| d.get("port")).and_then(status_code).unwrap_or(5667);
            let new_refer_to =
                instruction.get("new_refer_to").and_then(|v| v.as_str()).map(str::to_string);
            let no_answer_ms = instruction.get("no_answer_ms").and_then(|v| v.as_i64());
            let callback_context =
                instruction.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
            let update_headers = parse_update_headers(instruction.get("update_headers"));
            ReferOutcome::Allow(CallReferResponse::Allow {
                destination: SipDestination::new(host, port),
                new_refer_to,
                update_headers,
                no_answer_ms,
                callback_context,
                label: parse_label(&instruction),
            })
        }
        _ => ReferOutcome::Allow(CallReferResponse::Reject {
            code: 603,
            reason: Some("Declined".into()),
            label: None,
        }),
    }
}

/// The `features.refer` arm a plan/destination object states —
/// `{"…","features":{"refer":{}}}`. Its PRESENCE directs LOCAL REFER
/// processing on the routed call; absent (or `null`), a REFER relays to the peer
/// leg like any other in-dialog method.
fn refer_feature_from_obj(obj: &serde_json::Value) -> Option<call::features::ReferFeature> {
    let refer = obj.get("features")?.get("refer")?;
    if refer.is_null() {
        return None;
    }
    Some(call::features::ReferFeature {
        max_chain_depth: refer.get("max_chain_depth").and_then(|v| v.as_i64()),
    })
}

/// The `features.refer` arm an inbound `X-Api-Call` states (the single-
/// destination shape, which is not a plan route). Absent header / non-JSON →
/// `None`.
fn refer_feature_from_api_call(req: &NewCallRequest) -> Option<call::features::ReferFeature> {
    refer_feature_from_obj(&parse_api_call_plan(req)?)
}

/// Parse an inbound `X-Api-Call` JSON header into call-limiter admission
/// entries — `{"...","call_limiter":[{"id":"x","limit":20}]}`. Absent header,
/// non-JSON, or a missing/!array `call_limiter` field all yield an empty vec
/// (no limiting). Entries missing `id`/`limit` are skipped.
pub fn limiter_entries_from_api_call(req: &NewCallRequest) -> Vec<LimiterEntry> {
    let raw = match req.sip_header("X-Api-Call") {
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
                    Some(LimiterEntry {
                        id: e.get("id")?.as_str()?.to_string(),
                        limit: e.get("limit")?.as_i64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an inbound `X-Api-Call` JSON header's `destination` object into a b-leg
/// target `(host, port)` — `{"destination":{"host":"…","port":5060}}`. This is
/// how a test UAC pins which callee (e.g. a specific headless-StatefulSet
/// `sipp-uas` pod, by its stable cluster-DNS name) the b-leg INVITE is sent to,
/// so call distribution is driven by the UAC rather than by kube-proxy ClusterIP
/// DNAT — Bob is then reached pod-direct from the LB VIP with no NAT. `host` may
/// be a DNS name; it is resolved to an IP literal in [`super::apply_route`]
/// before it reaches the wire. Absent header / non-JSON / missing or empty
/// `destination.host` → `None` (caller falls back to the configured `B2BUA_DEST`).
/// `port` defaults to 5060 when omitted.
pub fn route_dest_from_api_call(req: &NewCallRequest) -> Option<(String, u16)> {
    let raw = req.sip_header("X-Api-Call")?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let dest = v.get("destination")?;
    let host = dest.get("host")?.as_str()?.trim().to_string();
    if host.is_empty() {
        return None;
    }
    let port = dest.get("port").and_then(status_code).unwrap_or(5060);
    Some((host, port))
}

/// Optional b-leg R-URI **userpart** from `X-Api-Call.destination.user`.
///
/// Lets a caller set ONLY the userpart of the b-leg Request-URI; the host:port
/// stays the validated `destination` (see [`route_dest_from_api_call`]). This is
/// how a downstream registrar front-proxy is given the AOR to resolve — it keys
/// its registered-contact lookup on the R-URI userpart, so the b-leg must read
/// `sip:<user>@<core-host>:<core-port>` (not the bare `sip:<core>` the LB emits
/// by default, which carries no AOR and is rejected). Absent / non-JSON / empty
/// → `None` (caller keeps the userless R-URI).
pub fn route_user_from_api_call(req: &NewCallRequest) -> Option<String> {
    let raw = req.sip_header("X-Api-Call")?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let user = v.get("destination")?.get("user")?.as_str()?.trim().to_string();
    if user.is_empty() {
        None
    } else {
        Some(user)
    }
}

/// The b-leg Request-URI for a call routed to `host:port`: the ROUTED TARGET,
/// carrying the userpart the a-leg named where it named one.
///
/// Keeping the userpart is what makes the rewrite lossless — the callee's own
/// identity is what a Request-URI is for, and a downstream registrar front
/// proxy resolves the AOR from it — while the host:port is non-negotiable (the
/// anti-loop invariant on [`route_to`]). URI reading is `sip-message`'s; this
/// module never parses one itself.
fn routed_ruri(req: &NewCallRequest, host: &str, port: u16) -> String {
    let user = sip_message::header::Uri::parse(&sip_message::SipStr::owned(&req.ruri))
        .ok()
        .and_then(|uri| uri.user().map(str::to_string));
    match user {
        Some(user) if !user.is_empty() => format!("sip:{user}@{host}:{port}"),
        _ => format!("sip:{host}:{port}"),
    }
}

/// Build a [`RouteDecision`] to `host:port` with default platform features.
///
/// The b-leg Request-URI names the ROUTED TARGET. Left unrewritten it defaults
/// to the a-leg's (`relay::originate`), which — the caller having dialled this
/// system's own ingress — is this system's own address: a Request-URI a strict
/// UAS rejects under RFC 3261 §8.1.2, and behind a front proxy the anti-loop
/// hazard `route_all_to_with_limiter` documents (a request forwarded to its own
/// R-URI bounces back to a worker, which re-INVITEs a fresh b-leg — one hop of
/// the inherited budget per bounce). A caller that means something else —
/// a userpart for a downstream registrar, a plan route's own rewrite — sets
/// `new_ruri` after this.
pub fn route_to(host: &str, port: u16) -> RouteDecision {
    RouteDecision {
        destination: SipDestination::new(host, port),
        new_ruri: Some(format!("sip:{host}:{port}")),
        new_from: None,
        new_to: None,
        update_headers: None,
        update_body: BodyUpdate::Keep,
        no_answer_ms: None,
        call_limiter: Vec::new(),
        callback_context: None,
        features: default_platform_features(),
        service_ext: Default::default(),
        subscriptions: Vec::new(),
        trace: false,
        label: None,
    }
}

/// Build a [`RouteDecision`] to `host:port` whose `features.refer` arm directs
/// LOCAL REFER processing — the scripted equivalent of the wire `features.refer`
/// field. Without it the routed call relays a REFER to its peer leg.
pub fn route_to_processing_refer(host: &str, port: u16) -> RouteDecision {
    let mut r = route_to(host, port);
    r.features.refer = Some(call::features::ReferFeature::default());
    r
}

/// Build a [`RouteDecision`] to `host:port` with the `relayFirst18xTo180`
/// feature active under `strategy` (the scripted equivalent of the wire
/// `relay_first_18x_to_180` field; `true`→`DropSdp` is the suppress mode).
pub fn route_to_with_18x(
    host: &str,
    port: u16,
    strategy: call::features::RelayFirst18xStrategy,
) -> RouteDecision {
    route_to_with_18x_messages(host, port, strategy, Default::default())
}

/// [`route_to_with_18x`] with an explicit [`call::features::Relay18xMessages`] policy (the
/// scripted equivalent of the feature's `messages` field).
pub fn route_to_with_18x_messages(
    host: &str,
    port: u16,
    strategy: call::features::RelayFirst18xStrategy,
    messages: call::features::Relay18xMessages,
) -> RouteDecision {
    let mut r = route_to(host, port);
    r.features.relay_first_18x_to_180 =
        Some(call::features::RelayFirst18xTo180Feature { strategy, messages });
    r
}

/// A reject decision: `code` and `reason`, with no header updates, service
/// slices or label.
pub fn reject(code: u16, reason: impl Into<String>) -> NewCallResponse {
    NewCallResponse::Reject(RejectDecision {
        reject_code: code,
        reject_reason: Some(reason.into()),
        update_headers: None,
        service_ext: Default::default(),
        label: None,
    })
}

type NewCallFallback = Box<dyn Fn(&NewCallRequest) -> NewCallResponse + Send + Sync>;
type FailureRule = Box<dyn Fn(&CallFailureRequest) -> CallFailureResponse + Send + Sync>;

pub struct ScriptedBuilder {
    rules: Vec<NewCallRule>,
    fallback: Option<NewCallFallback>,
    failure: Option<FailureRule>,
    refer: Option<ReferRule>,
    release: Option<ReleaseRule>,
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

    /// Script the `call_release` consult (subscribed release events).
    /// Unset = the trait's back-compat default (`Release` —
    /// local teardown).
    pub fn on_release(
        mut self,
        f: impl Fn(&CallReleaseRequest) -> ReleaseOutcome + Send + Sync + 'static,
    ) -> Self {
        self.release = Some(Box::new(f));
        self
    }

    pub fn build(self) -> ScriptedDecisionEngine {
        ScriptedDecisionEngine {
            rules: self.rules,
            fallback: self.fallback.unwrap_or_else(|| Box::new(|_| reject(404, "Not Found"))),
            failure: self
                .failure
                .unwrap_or_else(|| Box::new(|_| CallTreatment::Relay { label: None })),
            release: self.release,
            refer: self.refer.unwrap_or_else(|| {
                // Default: transfer is dormant — reject 501.
                Box::new(|_| {
                    ReferOutcome::Allow(CallReferResponse::Reject {
                        code: 501,
                        reason: Some("Not Implemented".into()),
                        label: None,
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

    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        match &self.release {
            // Unscripted: the trait's back-compat default (local teardown).
            None => {
                Ok(CallReleaseResponse::Release { label: None, service_ext: Default::default() })
            }
            Some(f) => match f(&req) {
                ReleaseOutcome::Respond(resp) => Ok(resp),
                ReleaseOutcome::Error => {
                    Err(CallDecisionError::Unavailable("scripted call_release error".into()))
                }
                // Hangs forever; the DeadlineDecisionEngine bound resolves it.
                ReleaseOutcome::Hang => std::future::pending().await,
            },
        }
    }
}

#[cfg(test)]
mod tests;
