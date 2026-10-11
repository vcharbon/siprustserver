//! The generic retry rule (ADR-0043): a request whose attempt failed at the
//! connection level (refused, unreachable, connect timeout, reset, EOF,
//! broken pipe) or timed out is sent once more, on the same client, when the
//! total budget leaves room for a whole attempt after the first was due to
//! end; the second gets that attempt's budget, at most the time left. Never after an HTTP response of any status,
//! never while the breaker is open, never for a request that was not sent.
//! The pool hands out the most recently returned idle connection first, so a
//! retry lands on a connection that has just proven itself.

use std::time::Duration;

use tokio::time::Instant;

use super::{AttemptError, BackendClient};
use crate::transport::{HttpRequest, HttpResponse};

/// The budgets of a retried request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budgets {
    /// One attempt's budget.
    pub attempt: Duration,
    /// Every attempt's, together.
    pub total: Duration,
}

impl BackendClient {
    /// Send `req` under `budgets`, once more after a retryable failure (see
    /// the module doc). The last attempt's outcome.
    pub async fn send_with_retry(
        &self,
        req: HttpRequest,
        budgets: Budgets,
    ) -> Result<HttpResponse, AttemptError> {
        let start = Instant::now();
        let deadline = start + budgets.total;
        let attempt = budgets.attempt.min(budgets.total);
        let client = self.inner.rotation.client();
        let first = self.attempt_on(&client, req.clone(), attempt).await;
        match first {
            Err(e) if e.is_retryable() && !self.is_open() => {
                // The room is read from where the first attempt was due to
                // end at the latest, not from the clock after it: a timer
                // fires a little past its deadline, and a total of two
                // attempts must still hold the second one.
                let now = Instant::now();
                let ended = now.min(start + attempt);
                if deadline.saturating_duration_since(ended) < attempt {
                    return Err(e);
                }
                super::metrics::retried(&self.inner.peer);
                let left = deadline.saturating_duration_since(now);
                self.attempt_on(&client, req, attempt.min(left)).await
            }
            outcome => outcome,
        }
    }
}
