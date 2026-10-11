//! The generic retry rule (ADR-0043): a request whose attempt failed at the
//! connection level (refused, unreachable, connect timeout, reset, EOF,
//! broken pipe) or timed out is sent once more, on the same client, while
//! the total budget has time left; the second attempt gets what remains,
//! at most one attempt's budget. Never after an HTTP response of any status,
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
        let deadline = Instant::now() + budgets.total;
        let client = self.inner.rotation.client();
        let first = self.attempt_on(&client, req.clone(), budgets.attempt.min(budgets.total)).await;
        match first {
            Err(e) if e.is_retryable() && !self.is_open() => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(e);
                }
                super::metrics::retried(&self.inner.peer);
                self.attempt_on(&client, req, budgets.attempt.min(left)).await
            }
            outcome => outcome,
        }
    }
}
