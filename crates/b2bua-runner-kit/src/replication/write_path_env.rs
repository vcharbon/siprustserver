//! The replication write path's settings a runner reads from env at boot
//! (ADR-0055 §3).
//!
//! ```text
//!   B2BUA_REPL_SYNC                     off | applied: when a turn's effects
//!                                       leave, relative to its write   (applied)
//!   B2BUA_REPL_SYNC_CAP_MS              the longest an outbox entry waits for
//!                                       the backup                         (100)
//!   B2BUA_REPL_BACKLOG_HIGH_ENTRIES     pending writes toward a backup at
//!                                       which new calls get none           (10000)
//!   B2BUA_REPL_BACKLOG_LOW_ENTRIES      … under which they get one again    (1000)
//!   B2BUA_REPL_BACKLOG_HIGH_AGE_MS      age of the oldest pending write at
//!                                       which new calls get none           (5000)
//!   B2BUA_REPL_BACKLOG_LOW_AGE_MS       … under which they get one again    (1000)
//! ```
//!
//! Unset takes the default. A level other than `off` or `applied`, or a value
//! that is not a positive integer, refuses the boot naming its variable, and
//! so does a cap not below T1 and the RFC 3261 §17.2.1 bound (200 ms), at
//! either level, or a low-water mark not under its high one.

use std::time::Duration;

use b2bua::repl::{BacklogMarks, SyncLevel, WritePath};
use sip_clock::wall::millis_u64;

use super::link_env::positive;

/// Reads the write path's variables through `get`; `Err` names the variable.
pub(crate) fn write_path(get: impl Fn(&str) -> Option<String>) -> Result<WritePath, String> {
    let d = WritePath::default();
    let ms = |key: &str, default: Duration| {
        positive(&get, key, millis_u64(default)).map(Duration::from_millis)
    };
    let count = |key: &str, default: usize| {
        let n = positive(&get, key, u64::try_from(default).unwrap_or(u64::MAX))?;
        usize::try_from(n).map_err(|_| format!("{key} is out of range"))
    };
    let sync = match get("B2BUA_REPL_SYNC") {
        Some(level) => {
            level.parse::<SyncLevel>().map_err(|why| format!("B2BUA_REPL_SYNC: {why}"))?
        }
        None => d.sync,
    };
    let path = WritePath {
        sync,
        sync_cap: ms("B2BUA_REPL_SYNC_CAP_MS", d.sync_cap)?,
        backlog: BacklogMarks {
            high_entries: count("B2BUA_REPL_BACKLOG_HIGH_ENTRIES", d.backlog.high_entries)?,
            low_entries: count("B2BUA_REPL_BACKLOG_LOW_ENTRIES", d.backlog.low_entries)?,
            high_age: ms("B2BUA_REPL_BACKLOG_HIGH_AGE_MS", d.backlog.high_age)?,
            low_age: ms("B2BUA_REPL_BACKLOG_LOW_AGE_MS", d.backlog.low_age)?,
        },
    };
    path.validate()
        .map_err(|why| format!("B2BUA_REPL_SYNC_CAP_MS / B2BUA_REPL_BACKLOG_*: {why}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<WritePath, String> {
        write_path(|key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn unset_takes_the_defaults() {
        assert_eq!(read(&[]), Ok(WritePath::default()));
    }

    #[test]
    fn each_variable_is_read() {
        let path = read(&[
            ("B2BUA_REPL_SYNC", "off"),
            ("B2BUA_REPL_SYNC_CAP_MS", "150"),
            ("B2BUA_REPL_BACKLOG_HIGH_ENTRIES", "50"),
            ("B2BUA_REPL_BACKLOG_LOW_ENTRIES", "5"),
            ("B2BUA_REPL_BACKLOG_HIGH_AGE_MS", "3000"),
            ("B2BUA_REPL_BACKLOG_LOW_AGE_MS", "300"),
        ])
        .unwrap();
        assert_eq!(path.sync, SyncLevel::Off);
        assert_eq!(path.sync_cap, Duration::from_millis(150));
        assert_eq!(
            path.backlog,
            BacklogMarks {
                high_entries: 50,
                low_entries: 5,
                high_age: Duration::from_secs(3),
                low_age: Duration::from_millis(300),
            }
        );
    }

    #[test]
    fn a_cap_not_below_the_trying_bound_refuses_the_boot() {
        assert!(read(&[("B2BUA_REPL_SYNC_CAP_MS", "200")]).is_err());
        assert!(read(&[("B2BUA_REPL_SYNC_CAP_MS", "500")]).is_err());
        assert!(read(&[("B2BUA_REPL_SYNC_CAP_MS", "0")]).is_err());
        assert!(read(&[("B2BUA_REPL_SYNC", "off"), ("B2BUA_REPL_SYNC_CAP_MS", "200")]).is_err());
    }

    #[test]
    fn a_level_other_than_off_or_applied_refuses_the_boot() {
        let err = read(&[("B2BUA_REPL_SYNC", "sent")]).unwrap_err();
        assert!(err.contains("B2BUA_REPL_SYNC"), "{err}");
    }

    #[test]
    fn marks_out_of_order_refuse_the_boot() {
        let err = read(&[("B2BUA_REPL_BACKLOG_LOW_ENTRIES", "20000")]).unwrap_err();
        assert!(err.contains("low-water"), "{err}");
    }
}
