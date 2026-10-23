//! The limiter's series as Prometheus text: every label value, zero included.

use std::time::Duration;

use super::{
    AdmitSite, LimiterCounters, LimiterOp, LimiterTask, RefreshDiscard, RefreshGiveUp,
    ReleaseGiveUp, REFRESH_OUTCOMES,
};
use crate::limiter::release_queue::ReleaseFlushOutcome;
use crate::metrics::cells::total;

impl LimiterCounters {
    /// Append every series as Prometheus text.
    pub fn render(&self, s: &mut String) {
        use crate::metrics::catalogue::limiter as c;
        let secs = |d: Duration| d.as_millis() as f64 / 1000.0;
        c::REQUESTS.render(s, |series| {
            total(&LimiterOp::ALL, series.index(&c::OP), |v| self.requests_total(v))
        });
        c::FAILURES.render(s, |series| {
            LimiterOp::ALL.get(series.block()).map_or(0, |&op| {
                total(op.causes(), series.at(1), |cause| self.failures_total(op, cause))
            })
        });
        c::ADMIT_RELEASED.render(s, |series| {
            total(&AdmitSite::ALL, series.index(&c::ADMIT_SITE), |v| self.admit_released_total(v))
        });
        c::UNCOUNTED_CALLS.render_value(s, self.uncounted_calls());
        c::REFRESH_ANSWERS.render(s, |series| {
            total(&REFRESH_OUTCOMES, series.index(&c::REFRESH_OUTCOME), |v| {
                self.refresh_answers_total(v)
            })
        });
        c::REFRESH_ANSWERS_DISCARDED.render(s, |series| {
            total(&RefreshDiscard::ALL, series.index(&c::REFRESH_DISCARD), |v| {
                self.refresh_discarded_total(v)
            })
        });
        c::REFRESH_KEYS_SENT.render_value(s, self.refresh_keys_sent_total());
        c::REFRESH_RETRIES.render_value(s, self.refresh_retries_total());
        c::REFRESH_GIVEN_UP.render(s, |series| {
            total(&RefreshGiveUp::ALL, series.index(&c::REFRESH_GIVE_UP), |v| {
                self.refresh_given_up_total(v)
            })
        });
        c::REFRESH_DUE.render_value(s, self.refresh_due());
        c::RELEASE_RETRIES.render_value(s, self.release_retries_total());
        c::RELEASE_GIVEN_UP.render(s, |series| {
            total(&ReleaseGiveUp::ALL, series.index(&c::RELEASE_GIVE_UP), |v| {
                self.release_given_up_total(v)
            })
        });
        c::RELEASE_QUEUE_DEPTH.render_value(s, self.release_queue_depth());
        c::RELEASE_FLUSHES.render(s, |series| {
            total(&ReleaseFlushOutcome::ALL, series.index(&c::RELEASE_FLUSH), |v| {
                self.release_flushes_total(v)
            })
        });
        c::RELEASE_FLUSH_SECONDS.render_value(s, secs(self.release_flush_time()));
        c::TASK_RESTARTS.render(s, |series| {
            total(&LimiterTask::ALL, series.index(&c::TASK), |v| self.task_restarts_total(v))
        });
        c::BREAKER_OPEN.render_value(s, self.breaker_open() as u64);
        c::BREAKER_TRANSITIONS
            .render(s, |series| self.breaker_transitions_total(series.index(&c::BREAKER_TO) == 0));
        c::LEASE_SECONDS.render_value(s, secs(self.lease()));
        c::REFRESH_PERIOD_SECONDS.render_value(s, secs(self.refresh_period()));
        c::LEASE_TOO_SHORT.render_value(s, self.lease_too_short_total());
        c::REFRESH_PERIOD_CLAMPED.render_value(s, self.refresh_period_clamped_total());
    }
}
