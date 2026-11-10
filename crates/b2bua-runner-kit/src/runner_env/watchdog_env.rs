//! The poll watchdog's env grammar (ADR-0057 §6).
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `B2BUA_STUCK_POLL_CPU_MS` | the CPU one poll may spend on one runtime thread before the worker discharges the call it serves and exits | 10000 |
//!
//! Only CPU the poll's thread spends inside that one poll counts: a
//! throttled, starved or blocked thread spends none, so box load never reads
//! as a stuck poll. Unset or blank takes the default; a value outside
//! [`MIN_STUCK_POLL_CPU`]..=[`MAX_STUCK_POLL_CPU`], or not a count of
//! milliseconds, refuses boot.

use std::time::Duration;

use crate::stated;

/// The default CPU a poll may spend before it is stuck.
pub const DEFAULT_STUCK_POLL_CPU: Duration = Duration::from_millis(10_000);

/// The lowest threshold: below it a healthy turn's synchronous work could
/// read stuck.
pub const MIN_STUCK_POLL_CPU: Duration = Duration::from_millis(100);

/// The highest threshold, ten minutes of one poll on the CPU.
pub const MAX_STUCK_POLL_CPU: Duration = Duration::from_millis(600_000);

const KEY: &str = "B2BUA_STUCK_POLL_CPU_MS";

/// The stuck threshold `lookup` states. `Err` names the variable.
pub(crate) fn stuck_poll_cpu_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Duration, String> {
    let Some(v) = stated(lookup(KEY)) else {
        return Ok(DEFAULT_STUCK_POLL_CPU);
    };
    let ms = v.trim().parse::<u64>().map_err(|e| format!("{KEY}={v:?}: {e}"))?;
    let cpu = Duration::from_millis(ms);
    if (MIN_STUCK_POLL_CPU..=MAX_STUCK_POLL_CPU).contains(&cpu) {
        return Ok(cpu);
    }
    Err(format!(
        "{KEY}={ms} is outside {}..={} ms",
        MIN_STUCK_POLL_CPU.as_millis(),
        MAX_STUCK_POLL_CPU.as_millis()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(v: Option<&str>) -> Result<Duration, String> {
        stuck_poll_cpu_from_lookup(|k| (k == KEY).then(|| v.map(str::to_string)).flatten())
    }

    #[test]
    fn unset_or_blank_takes_the_default() {
        assert_eq!(read(None), Ok(DEFAULT_STUCK_POLL_CPU));
        assert_eq!(read(Some(" ")), Ok(DEFAULT_STUCK_POLL_CPU));
        assert_eq!(read(Some("200")), Ok(Duration::from_millis(200)));
    }

    #[test]
    fn a_value_out_of_range_or_not_a_count_refuses_boot() {
        for bad in ["0", "99", "600001", "ten", "-5"] {
            let err = read(Some(bad)).expect_err(bad);
            assert!(err.contains(KEY), "{err}");
        }
    }
}
