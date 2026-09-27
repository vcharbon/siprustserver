//! Wire DTOs for the call-keyed limiter HTTP API.
//!
//! Endpoints:
//! - `POST /v1/admit`   [`AdmitRequest`]  -> [`AdmitResponse`]
//! - `POST /v1/release` [`ReleaseRequest`] -> `200 {}`
//! - `POST /v1/refresh` [`RefreshRequest`] -> [`RefreshResponse`]
//!
//! Every request names the call (`call_ref`); the server keeps the call's set
//! and its lease, so the client stores nothing but whether the call is
//! counted.

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
    pub call_ref: String,
    /// Every limiter entry the call must satisfy, admitted all-or-none.
    pub entries: Vec<AdmitEntry>,
    /// On a cap refusal, drop the call's current set in the same step.
    #[serde(default)]
    pub release_on_refusal: bool,
}

/// `POST /v1/admit` response. `admitted` carries the whole set; a cap
/// refusal names the first `rejected_id` at its cap; `released` states a
/// refusal by the call's tombstone (the call ended), which names no id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitResponse {
    /// Whether the call's set is now the entries sent.
    pub admitted: bool,
    /// The first id at its cap (present iff refused on a cap).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rejected_id: Option<String>,
    /// The call was released within the last lease: nothing is held for it.
    #[serde(default)]
    pub released: bool,
}

/// `POST /v1/release` body: drop the call's set. Idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// The call to release.
    pub call_ref: String,
}

/// `POST /v1/refresh` body: extend the call's lease, or re-create its set from
/// `ids` when the store no longer holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// The call to keep alive.
    pub call_ref: String,
    /// The ids the call holds, re-registered when the store holds no set.
    #[serde(default)]
    pub ids: Vec<String>,
}

/// `POST /v1/refresh` response. `known`: the lease was extended;
/// `reregistered`: the set was re-created from the ids sent; neither: the call
/// was released within the last lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub known: bool,
    #[serde(default)]
    pub reregistered: bool,
}
