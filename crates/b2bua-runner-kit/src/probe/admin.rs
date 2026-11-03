//! The worker's operator routes on its loopback operator server
//! (`B2BUA_ADMIN`, ADR-0050 §10):
//!
//! - `POST /admin/cordon` cordons the worker, `DELETE /admin/cordon` lifts the
//!   cordon: `204`, idempotent; `409` with the state the worker is in when no
//!   cordon applies to it (NotReady, Draining).
//! - `GET /admin/cordon`: `200` with `{"state", "cordoned", "calls"}`, the
//!   readiness state, whether a cordon is set, and the live call count.
//! - `GET /admin/calls`: `200` with `{"calls": [{"call_ref", "call_ids"}]}`,
//!   every live call by its ref and the Call-IDs of its legs, a-leg first;
//!   `503` when the worker's calls cannot be read.
//! - `POST /admin/release-all` releases every call the worker serves
//!   ([`b2bua::release_all`]): `200` with `{"released", "aborted"}`, the
//!   answered calls released and the calls in setup aborted. The calls are
//!   released on their own turns after the answer.

use std::sync::Arc;

use b2bua::release_all::ReleaseAllFn;
use b2bua::repl::{CordonRefused, Readiness};
use probe_http::{AdminFn, AdminReply};

/// The live call count the routes report.
pub(crate) type CallCount = Arc<dyn Fn() -> usize + Send + Sync>;

/// One live call as `GET /admin/calls` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveCall {
    pub call_ref: String,
    /// The Call-ID of each of its legs, a-leg first.
    pub call_ids: Vec<String>,
}

/// The live calls the routes list; `None` when they cannot be read.
pub(crate) type LiveCallsFn = Arc<dyn Fn() -> Option<Vec<LiveCall>> + Send + Sync>;

/// The operator routes of a worker with `readiness`, reporting `calls`,
/// listing `live`, whose calls `release_all` releases.
pub(crate) fn admin_routes(
    readiness: Readiness,
    calls: CallCount,
    live: LiveCallsFn,
    release_all: ReleaseAllFn,
) -> AdminFn {
    Arc::new(move |method, path| match path {
        "/admin/cordon" => Some(cordon(&readiness, &calls, method)),
        "/admin/calls" => Some(listed(&live, method)),
        "/admin/release-all" => Some(release(&release_all, method)),
        _ => None,
    })
}

fn cordon(readiness: &Readiness, calls: &CallCount, method: &str) -> AdminReply {
    match method {
        "POST" => done(readiness.cordon()),
        "DELETE" => done(readiness.uncordon()),
        "GET" => AdminReply::json(
            200,
            serde_json::json!({
                "state": readiness.state().label(),
                "cordoned": readiness.is_cordoned(),
                "calls": calls(),
            })
            .to_string(),
        ),
        _ => AdminReply::method_not_allowed("GET, POST, DELETE"),
    }
}

fn listed(live: &LiveCallsFn, method: &str) -> AdminReply {
    match method {
        "GET" => {
            let Some(live) = live() else {
                return AdminReply::json(
                    503,
                    serde_json::json!({ "error": "the worker's calls cannot be read" }).to_string(),
                );
            };
            let calls: Vec<serde_json::Value> = live
                .into_iter()
                .map(|c| serde_json::json!({ "call_ref": c.call_ref, "call_ids": c.call_ids }))
                .collect();
            AdminReply::json(200, serde_json::json!({ "calls": calls }).to_string())
        }
        _ => AdminReply::method_not_allowed("GET"),
    }
}

fn release(release_all: &ReleaseAllFn, method: &str) -> AdminReply {
    match method {
        "POST" => {
            let reached = release_all();
            AdminReply::json(200, serde_json::to_string(&reached).unwrap_or_default())
        }
        _ => AdminReply::method_not_allowed("POST"),
    }
}

fn done(outcome: Result<(), CordonRefused>) -> AdminReply {
    match outcome {
        Ok(()) => AdminReply::no_content(),
        Err(refused) => {
            AdminReply::json(409, serde_json::json!({ "state": refused.label() }).to_string())
        }
    }
}

#[cfg(test)]
mod tests;
