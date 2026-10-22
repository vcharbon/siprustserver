//! The datagrams the parser refused or panicked on: dropped unanswered,
//! answered with a stateless 400, or dropped by the cap on those 400s.

use std::sync::atomic::{AtomicU64, Ordering};

use super::super::catalogue as c;
use super::ProxyMetrics;

#[derive(Default)]
pub(super) struct ParseCounts {
    panics: AtomicU64,
    errors: AtomicU64,
    answered: AtomicU64,
    capped: AtomicU64,
}

impl ParseCounts {
    pub(super) fn render(&self, s: &mut String) {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        c::PARSE_PANICS.render_value(s, load(&self.panics));
        c::PARSE_ERRORS.render_value(s, load(&self.errors));
        c::PARSE_REFUSALS_ANSWERED.render_value(s, load(&self.answered));
        c::PARSE_REFUSALS_CAPPED.render_value(s, load(&self.capped));
    }
}

impl ProxyMetrics {
    /// One inbound datagram the parser panicked on, dropped.
    pub fn record_parse_panic(&self) {
        self.parse.panics.fetch_add(1, Ordering::Relaxed);
    }

    /// Inbound datagrams the parser panicked on (counter).
    pub fn parse_panics(&self) -> u64 {
        self.parse.panics.load(Ordering::Relaxed)
    }

    /// One inbound datagram the parser refused, dropped unanswered.
    pub fn record_parse_error(&self) {
        self.parse.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Inbound datagrams the parser refused, dropped unanswered (counter).
    pub fn parse_errors(&self) -> u64 {
        self.parse.errors.load(Ordering::Relaxed)
    }

    /// One refused request answered with a stateless 400.
    pub fn record_parse_refusal_answered(&self) {
        self.parse.answered.fetch_add(1, Ordering::Relaxed);
    }

    /// Refused requests answered with a stateless 400 (counter).
    pub fn parse_refusals_answered(&self) -> u64 {
        self.parse.answered.load(Ordering::Relaxed)
    }

    /// One answerable refused request dropped by the cap on 400s.
    pub fn record_parse_refusal_capped(&self) {
        self.parse.capped.fetch_add(1, Ordering::Relaxed);
    }

    /// Answerable refused requests dropped by the cap on 400s (counter).
    pub fn parse_refusals_capped(&self) -> u64 {
        self.parse.capped.load(Ordering::Relaxed)
    }
}
