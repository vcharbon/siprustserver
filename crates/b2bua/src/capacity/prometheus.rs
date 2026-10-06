//! Prometheus text exposition of the capacity gate: its ceilings, its last
//! RSS sample and level, and the backup replicas it kept out.

use std::fmt;

use super::gate::{BackupBound, CapacityGate};
use crate::metrics::catalogue::capacity as c;

impl CapacityGate {
    /// The gate's series, appended to the worker's `/metrics` body (a new
    /// call it refused is counted on `b2bua_new_calls_total`):
    ///   - `b2bua_capacity_level` (gauge): 0 open, 1 non-emergency calls
    ///     refused, 2 every new call refused, as of the last sample.
    ///   - `b2bua_capacity_rss_bytes` (gauge): the RSS the gate last read;
    ///     NaN before the first reading.
    ///   - `b2bua_capacity_ceiling{bound,class}` (gauge): each ceiling, the
    ///     backup ones under `class="backup"` with the `bound` values of
    ///     `b2bua_repl_backup_shed_total`; `+Inf` where none is configured.
    ///   - `b2bua_repl_backup_shed_total{bound}` (counter): backup replicas
    ///     not stored.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(2048);
        c::CAPACITY_LEVEL.render_value(&mut s, self.level() as u8);
        c::CAPACITY_RSS_BYTES.render_value(&mut s, Reading(self.rss_bytes(), "NaN"));
        let limits = self.limits();
        c::CAPACITY_CEILING.render(&mut s, |series| {
            let ceiling = if series.block() == 0 {
                let bound = match series.index(&c::CEILING_BOUND) {
                    0 => limits.calls,
                    1 => limits.transactions,
                    _ => limits.rss_bytes,
                };
                match series.index(&c::CEILING_CLASS) {
                    0 => bound.normal,
                    _ => bound.emergency,
                }
            } else {
                match BackupBound::ALL[series.index(&c::BACKUP_BOUND)] {
                    BackupBound::Calls => limits.backup_calls,
                    BackupBound::Rss => limits.backup_rss_bytes,
                }
            };
            Reading(ceiling, "+Inf")
        });
        c::REPL_BACKUP_SHED.render(&mut s, |series| {
            self.backup_shed_total(BackupBound::ALL[series.index(&c::BACKUP_BOUND)])
        });
        s
    }
}

/// A sample that may hold no reading: its value, or the text standing for
/// none (`+Inf` for a ceiling not configured, `NaN` before a first sample).
struct Reading(Option<u64>, &'static str);

impl fmt::Display for Reading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(v) => write!(f, "{v}"),
            None => f.write_str(self.1),
        }
    }
}
