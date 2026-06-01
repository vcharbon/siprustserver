//! Wire DTOs for the batched/transactional limiter HTTP API.
//!
//! Endpoints:
//! - `POST /v1/admit`   [`AdmitRequest`]  -> [`AdmitResponse`]
//! - `POST /v1/release` [`ReleaseRequest`] -> `200 {}`
//! - `POST /v1/refresh` [`RefreshRequest`] -> [`RefreshResponse`]
//!
//! The server owns the clock, so it computes and returns the window timestamp;
//! the client stores it and echoes it back on release/refresh.

use serde::{Deserialize, Serialize};

/// One limiter entry to admit: an id and its cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// `POST /v1/admit` body: all entries for one call. Admitted all-or-none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    /// Every limiter entry the call must satisfy.
    pub entries: Vec<AdmitEntry>,
}

/// `POST /v1/admit` response. `admitted == true` carries the shared `window`
/// every entry was incremented in; `admitted == false` carries the first
/// `rejected_id` that was over cap (nothing was incremented).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitResponse {
    /// Whether all entries were admitted (and thus incremented).
    pub admitted: bool,
    /// The window timestamp all entries were incremented in (present iff admitted).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<i64>,
    /// The first id that was over cap (present iff rejected).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rejected_id: Option<String>,
}

/// One recorded hold: an id and the window it was incremented in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// The limiter id.
    pub id: String,
    /// The window timestamp the increment landed in.
    pub window: i64,
}

/// `POST /v1/release` body: decrement each hold's window (floored at 0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// Holds to release.
    pub entries: Vec<Hold>,
}

/// `POST /v1/refresh` body: migrate each hold from its window to the current one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// Holds to refresh.
    pub entries: Vec<Hold>,
}

/// `POST /v1/refresh` response: each hold with its new (current) window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshResponse {
    /// Holds with updated windows.
    pub entries: Vec<Hold>,
}
