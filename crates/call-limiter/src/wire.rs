//! Wire DTOs for the call-keyed limiter HTTP API.
//!
//! Endpoints:
//! - `POST /v1/admit`   [`AdmitRequest`]  -> [`AdmitResponse`]
//! - `POST /v1/release` [`ReleaseRequest`] -> `200 {}`
//! - `POST /v1/refresh` [`RefreshRequest`] -> [`RefreshResponse`] (many calls)
//! - `GET /v1/health` -> [`HealthResponse`]
//!
//! Every request names the call by the client's per-call limiter `key`, unique
//! over time; the server keeps the call's set and its lease, so the client
//! stores nothing but whether the call is counted. Every admit and refresh
//! answer states the server's lease (`lease_ms`), so a client bounds what it
//! keeps for the server by the lease the server runs.

use serde::{Deserialize, Serialize};

/// One limiter entry to admit: an id and its cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// `POST /v1/admit` body: the call's whole set, replacing what it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    /// The call the set belongs to.
    pub key: String,
    /// Every limiter entry the call must satisfy, admitted all-or-none.
    pub entries: Vec<AdmitEntry>,
    /// On a cap refusal, drop the call's current set in the same step.
    pub release_on_refusal: bool,
}

/// `POST /v1/admit` response. `admitted` carries the whole set; a cap
/// refusal names the first `rejected_id` at its cap; `released` states a
/// refusal by the call's release fence (the call ended), which names no id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitResponse {
    /// Whether the call's set is now the entries sent.
    pub admitted: bool,
    /// The first id at its cap (present iff refused on a cap).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rejected_id: Option<String>,
    /// The call was released within the last lease: nothing is held for it.
    pub released: bool,
    /// The server's lease, milliseconds: how long a set lives without a
    /// refresh, and a released key stays fenced.
    pub lease_ms: u64,
}

/// `POST /v1/release` body: drop the set of every call named, in one step.
/// Idempotent per key; a key the server holds nothing for changes no count,
/// creates no set and is fenced like any released call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// The calls to release.
    pub keys: Vec<String>,
}

/// One call a refresh names: extend its lease, or re-create its set from
/// `ids` when the store no longer holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshCall {
    /// The call to keep alive.
    pub key: String,
    /// The ids the call holds, re-registered when the store holds no set.
    pub ids: Vec<String>,
}

/// `POST /v1/refresh` body: every call to refresh, each on its own terms,
/// in one step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// The calls, in the order the answer states their outcomes.
    pub calls: Vec<RefreshCall>,
}

/// `POST /v1/refresh` response: the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshAnswer {
    /// The lease was extended.
    Extended,
    /// The set was re-created from the ids sent.
    Reregistered,
    /// Nothing is held for the call and nothing was re-created (released, or
    /// no ids sent).
    Released,
    /// Nothing is held for the call and nothing was re-created: an admit of
    /// the key dropped its set, and no admit since replaced it.
    Dropped,
}

/// `POST /v1/refresh` response: one outcome per call named, in the order of
/// the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub outcomes: Vec<RefreshAnswer>,
    /// The server's lease, milliseconds: how long a set lives without a
    /// refresh, and a released key stays fenced.
    pub lease_ms: u64,
}

/// `GET /v1/health` response: the store answered, holding `calls` sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Calls holding a set when the store answered.
    pub calls: u64,
}
