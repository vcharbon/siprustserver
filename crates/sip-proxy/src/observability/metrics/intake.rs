//! The datagrams the receive path drops at or above the intake watermark:
//! new non-emergency INVITEs shed, and datagrams no response can answer.

use std::sync::atomic::{AtomicU64, Ordering};

use super::super::catalogue as c;
use super::ProxyMetrics;

#[derive(Default)]
pub(super) struct IntakeCounts {
    shed: AtomicU64,
    unanswerable: AtomicU64,
}

impl IntakeCounts {
    pub(super) fn render(&self, s: &mut String) {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        c::INTAKE_SHED.render_value(s, load(&self.shed));
        c::INTAKE_UNANSWERABLE.render_value(s, load(&self.unanswerable));
    }
}

impl ProxyMetrics {
    /// One new non-emergency INVITE dropped by the intake shed.
    pub fn record_intake_shed(&self) {
        self.intake.shed.fetch_add(1, Ordering::Relaxed);
    }

    /// New non-emergency INVITEs dropped by the intake shed (counter).
    pub fn intake_shed(&self) -> u64 {
        self.intake.shed.load(Ordering::Relaxed)
    }

    /// One datagram no response can answer dropped at intake; the caller
    /// also counts it as a parse error or a parser panic.
    pub fn record_intake_unanswerable(&self) {
        self.intake.unanswerable.fetch_add(1, Ordering::Relaxed);
    }

    /// Datagrams no response can answer dropped at intake (counter).
    pub fn intake_unanswerable(&self) -> u64 {
        self.intake.unanswerable.load(Ordering::Relaxed)
    }
}
