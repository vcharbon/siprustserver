//! The worker's operator routes on its probe server (ADR-0050 §10):
//!
//! - `POST /admin/cordon` cordons the worker, `DELETE /admin/cordon` lifts the
//!   cordon: `204`, idempotent; `409` with the state the worker is in when no
//!   cordon applies to it (NotReady, Draining).
//! - `GET /admin/cordon`: `200` with `{"state", "cordoned", "calls"}`, the
//!   readiness state, whether a cordon is set, and the live call count.
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

/// The operator routes of a worker with `readiness`, reporting `calls`, whose
/// calls `release_all` releases.
pub(crate) fn admin_routes(
    readiness: Readiness,
    calls: CallCount,
    release_all: ReleaseAllFn,
) -> AdminFn {
    Arc::new(move |method, path| match path {
        "/admin/cordon" => Some(cordon(&readiness, &calls, method)),
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
