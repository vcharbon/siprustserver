//! Prometheus text exposition of the capacity gate: its ceilings, its last
//! RSS sample and level, and what it refused.

use std::fmt::Write;

use super::gate::{BackupBound, Bound, CapacityGate};

impl CapacityGate {
    /// The gate's series, appended to the worker's `/metrics` body:
    ///   - `b2bua_capacity_rejected_total{bound,class}` (counter): new calls
    ///     refused, by the bound met and the call's class
    ///     (`normal`/`emergency`).
    ///   - `b2bua_capacity_level` (gauge): 0 open, 1 non-emergency calls
    ///     refused, 2 every new call refused, as of the last sample.
    ///   - `b2bua_capacity_rss_bytes` (gauge): the RSS the gate last read;
    ///     absent without a reading.
    ///   - `b2bua_capacity_ceiling{bound,class}` (gauge): each configured
    ///     ceiling, the backup ones under `class="backup"` with the `bound`
    ///     values of `b2bua_repl_backup_shed_total`; an unset one is absent.
    ///   - `b2bua_repl_backup_shed_total{bound}` (counter): backup replicas
    ///     not stored.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str(
            "# HELP b2bua_capacity_rejected_total New calls refused with a 503 by a memory bound.\n\
             # TYPE b2bua_capacity_rejected_total counter\n",
        );
        for bound in Bound::ALL {
            for (class, emergency) in [("normal", false), ("emergency", true)] {
                let _ = writeln!(
                    s,
                    "b2bua_capacity_rejected_total{{bound=\"{}\",class=\"{class}\"}} {}",
                    bound.as_str(),
                    self.rejected_total(bound, emergency)
                );
            }
        }
        let _ = write!(
            s,
            "# HELP b2bua_capacity_level 0 open, 1 non-emergency calls refused, 2 every new call refused.\n\
             # TYPE b2bua_capacity_level gauge\nb2bua_capacity_level {}\n",
            self.level() as u8
        );
        if let Some(rss) = self.rss_bytes() {
            let _ = write!(
                s,
                "# HELP b2bua_capacity_rss_bytes Process RSS the capacity gate last sampled.\n\
                 # TYPE b2bua_capacity_rss_bytes gauge\nb2bua_capacity_rss_bytes {rss}\n"
            );
        }
        s.push_str(
            "# HELP b2bua_capacity_ceiling Configured capacity ceilings.\n\
             # TYPE b2bua_capacity_ceiling gauge\n",
        );
        let limits = self.limits();
        let ceilings = [
            ("calls", limits.calls),
            ("transactions", limits.transactions),
            ("rss", limits.rss_bytes),
        ];
        for (bound, c) in ceilings {
            for (class, v) in [("normal", c.normal), ("emergency", c.emergency)] {
                if let Some(v) = v {
                    let _ = writeln!(
                        s,
                        "b2bua_capacity_ceiling{{bound=\"{bound}\",class=\"{class}\"}} {v}"
                    );
                }
            }
        }
        let backup = [
            (BackupBound::Calls, limits.backup_calls),
            (BackupBound::Rss, limits.backup_rss_bytes),
        ];
        for (bound, v) in backup {
            if let Some(v) = v {
                let _ = writeln!(
                    s,
                    "b2bua_capacity_ceiling{{bound=\"{}\",class=\"backup\"}} {v}",
                    bound.as_str()
                );
            }
        }
        s.push_str(
            "# HELP b2bua_repl_backup_shed_total Backup replicas not stored because a backup ceiling was reached.\n\
             # TYPE b2bua_repl_backup_shed_total counter\n",
        );
        for bound in BackupBound::ALL {
            let _ = writeln!(
                s,
                "b2bua_repl_backup_shed_total{{bound=\"{}\"}} {}",
                bound.as_str(),
                self.backup_shed_total(bound)
            );
        }
        s
    }
}
