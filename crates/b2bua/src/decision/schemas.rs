//! Request/response shapes for the call-decision adapter — port of
//! `src/decision/schemas/*`. `FeatureActivations` is reused from `call` (it is
//! the canonical feature shape; the data model already carries it on `Call`).
//!
//! Only [`NewCallRequest`] derives serde — it is what the future HTTP adapter
//! POSTs and what the scripted test adapter inspects. The response types are
//! constructed in-process (by the scripted adapter / a future JSON decoder), so
//! they stay plain Rust values here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use call::features::FeatureActivations;

/// Header name → value, or `None` to delete the header.
pub type SipHeaderUpdates = BTreeMap<String, Option<String>>;

/// The decision request sent on a new INVITE (the call context the backend
/// keys decisions off — R-URI, From/To, all non-structural `X-*` headers, body).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewCallRequest {
    pub call_id: String,
    pub ruri: String,
    pub from: String,
    pub to: String,
    pub via: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub contact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_type: Option<String>,
    /// All headers not sent as a top-level field (this is where `X-*` land).
    #[serde(default)]
    pub sip_headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sip_body: Option<String>,
}

/// A downstream SIP peer.
#[derive(Debug, Clone)]
pub struct SipDestination {
    pub host: String,
    pub port: Option<u16>,
    pub transport: Option<String>,
}

impl SipDestination {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port: Some(port),
            transport: None,
        }
    }
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(5060)
    }
}

/// Three-way body directive on a route: leave the inbound body, drop it, or
/// substitute a new one (the source's `update_body` absent/null/value).
#[derive(Debug, Clone, Default)]
pub enum BodyUpdate {
    #[default]
    Keep,
    Drop,
    Replace(String),
}

#[derive(Debug, Clone)]
pub struct CallLimiterEntry {
    pub id: String,
    pub limit: i64,
}

/// A "route" decision — bridge the call to `destination`.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub destination: SipDestination,
    pub new_ruri: Option<String>,
    pub update_headers: Option<SipHeaderUpdates>,
    pub update_body: BodyUpdate,
    pub no_answer_timeout_sec: Option<i64>,
    pub call_limiter: Vec<CallLimiterEntry>,
    pub callback_context: Option<String>,
    pub features: FeatureActivations,
    pub service_ext: BTreeMap<String, serde_json::Value>,
}

/// A "reject" decision — answer the INVITE with a failure response.
#[derive(Debug, Clone)]
pub struct RejectDecision {
    pub reject_code: u16,
    pub reject_reason: Option<String>,
    pub update_headers: Option<SipHeaderUpdates>,
}

/// Response to [`NewCallRequest`].
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Route is the hot path; boxing adds an alloc per call.
pub enum NewCallResponse {
    Route(RouteDecision),
    Reject(RejectDecision),
}

/// What failed on a b-leg, for the failover decision.
#[derive(Debug, Clone)]
pub struct FailureInfo {
    pub origin: String,
    pub status_code: Option<u16>,
    pub limiter_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CallFailureRequest {
    pub callback_context: Option<String>,
    pub failure: FailureInfo,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum CallFailureResponse {
    Failover(RouteDecision),
    Terminate,
}

#[derive(Debug, Clone)]
pub struct CallReferRequest {
    pub callback_context: Option<String>,
    pub refer_to: String,
}

#[derive(Debug, Clone)]
pub enum CallReferResponse {
    Allow { destination: SipDestination },
    Reject { code: u16, reason: Option<String> },
}

/// Mandatory platform features every successful route must carry (the source
/// 500s if `features` is absent). A sane default for the scripted adapter.
pub fn default_platform_features() -> FeatureActivations {
    use call::features::{KeepaliveActivation, PlatformActivations};
    FeatureActivations {
        platform: PlatformActivations {
            max_duration_sec: 3_600,
            keepalive: KeepaliveActivation {
                interval_sec: 30,
                max_missed: 2,
            },
        },
        refer: None,
        relay_first_18x_to_180: None,
        no_answer_timeout_sec: None,
        call_limiters: None,
    }
}
