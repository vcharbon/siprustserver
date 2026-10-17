//! The worker's operator routes on its probe server (ADR-0050 §10):
//!
//! - `POST /admin/cordon` cordons the worker, `DELETE /admin/cordon` lifts the
//!   cordon: `204`, idempotent; `409` with the state the worker is in when no
//!   cordon applies to it (NotReady, Draining).
//! - `GET /admin/cordon`: `200` with `{"state", "cordoned", "calls"}`, the
//!   readiness state, whether a cordon is set, and the live call count.

use std::sync::Arc;

use b2bua::repl::{CordonRefused, Readiness};
use probe_http::{AdminFn, AdminReply};

/// The live call count the routes report.
pub(crate) type CallCount = Arc<dyn Fn() -> usize + Send + Sync>;

/// The operator routes of a worker with `readiness`, reporting `calls`.
pub(crate) fn admin_routes(readiness: Readiness, calls: CallCount) -> AdminFn {
    Arc::new(move |method, path| match path {
        "/admin/cordon" => Some(cordon(&readiness, &calls, method)),
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
