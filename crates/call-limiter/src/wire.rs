//! Wire DTOs for the call-keyed limiter HTTP API.
//!
//! Endpoints:
//! - `POST /v1/admit`   [`AdmitRequest`]  -> [`AdmitResponse`]
//! - `POST /v1/release` [`ReleaseRequest`] -> `200 {}`
//! - `POST /v1/refresh` [`RefreshRequest`] -> [`RefreshResponse`] (many calls)
//! - `GET /v1/health` -> [`HealthResponse`]
//!
//! Every request names the call by the client's per-call limiter `key`, unique
//! over time; the server keeps the call's set and its lease. Every admit
//! carries the call's change number, and the server refuses one not above the
//! number of the set it holds for the key ([`AdmitAnswer::Superseded`]), so a
//! late or repeated admit never overwrites a newer set. Every admit also
//! carries the set the call last learnt it holds, which a server that knows
//! nothing of the key re-registers before it checks the change. Every admit and
//! refresh answer states the set the server holds for the key ([`HeldSet`]),
//! so a client that lost an answer learns the truth from the next one. Every
//! admit, refresh and health answer states the server's lease (`lease_ms`), so
//! a client bounds what it keeps for the server by the lease the server runs.

use serde::{Deserialize, Serialize};

/// One limiter entry to admit: an id and its cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// The set the server holds for a key: its entries (empty when it holds
/// none) and the change number of the last admit it answered for the key (0
/// when it knows none).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldSet {
    /// The change number the set is stated under.
    pub change: u64,
    /// The entries held, in the order they were admitted.
    pub entries: Vec<AdmitEntry>,
}

/// `POST /v1/admit` body: the call's whole set, replacing what it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    /// The call the set belongs to.
    pub key: String,
    /// The call's change number for this admit: above every number the call
    /// reserved before for the key (a takeover copy may reuse one its dead
    /// node sent: the answer is then `superseded`, stating the number).
    pub change: u64,
    /// The set the call last learnt the server holds for the key, under its
    /// number (empty under 0 when it learnt none). A server that holds no set
    /// for the key and has not fenced it re-registers it, with no cap check,
    /// as a refresh carrying it would, before it checks this admit.
    pub held: HeldSet,
    /// Every limiter entry the call must satisfy, admitted all-or-none.
    pub entries: Vec<AdmitEntry>,
    /// On a cap refusal, drop the call's current set in the same step.
    pub release_on_refusal: bool,
}

/// The outcome of one admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmitAnswer {
    /// The call's set is now the entries sent.
    Admitted,
    /// An id the call adds is at its cap: nothing moved but the drop
    /// `release_on_refusal` asked for.
    Rejected,
    /// The admit's change number is not above the one of the set the server
    /// holds for the key: nothing moved.
    Superseded,
    /// The call was released within the last lease: nothing is held for it.
    Released,
}

/// `POST /v1/admit` response. A cap refusal names the first `rejected_id` at
/// its cap. Every outcome but `released` states the set the server holds for
/// the key after the admit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitResponse {
    /// What the admit did.
    pub outcome: AdmitAnswer,
    /// The first id at its cap (present iff refused on a cap).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rejected_id: Option<String>,
    /// The set the server holds for the key (absent iff `released`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub held: Option<HeldSet>,
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
/// `entries` under `change` when the store no longer holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshCall {
    /// The call to keep alive.
    pub key: String,
    /// The change number of the set the call last learnt it holds.
    pub change: u64,
    /// The entries the call last learnt it holds, re-registered when the
    /// store holds no set.
    pub entries: Vec<AdmitEntry>,
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

/// One call's refresh answer: the outcome and, unless the call was released,
/// the set the server holds for the key after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshReply {
    /// What the refresh did.
    pub outcome: RefreshAnswer,
    /// The set the server holds for the key (absent iff `released`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub held: Option<HeldSet>,
}

/// `POST /v1/refresh` response: one reply per call named, in the order of
/// the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub outcomes: Vec<RefreshReply>,
    /// The server's lease, milliseconds: how long a set lives without a
    /// refresh, and a released key stays fenced.
    pub lease_ms: u64,
}

/// `GET /v1/health` response: the store answered, holding `calls` sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Calls holding a set when the store answered.
    pub calls: u64,
    /// The server's lease, milliseconds.
    pub lease_ms: u64,
}
