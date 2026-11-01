//! The event-queue gate's env grammar (ADR-0050 §9): when a worker whose
//! transaction event queue is backed up refuses new calls, on its OPTIONS
//! answer (`new=refuse; cause=overload`) and at its admission ladder's
//! event-queue rung (ADR-0037 item 11, a `503` to a new normal INVITE), and
//! when it accepts them again. Both read the one gate: one state, one set of
//! bounds, one hold.
//!
//! | variable | role | default |
//! |---|---|---|
//! | `B2BUA_EVENT_QUEUE_REFUSE_DEPTH` | queued events at which new calls are refused | 512 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_DEPTH` | queued events at or below which they may be accepted again | 64 |
//! | `B2BUA_EVENT_QUEUE_REFUSE_SOJOURN_MS` | a wait in the queue, ms, at which new calls are refused | 500 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_SOJOURN_MS` | a wait in the queue, ms, at or below which they may be accepted again | 50 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_HOLD_MS` | how long both accept bounds hold before new calls are accepted again | 3000 |
//!
//! The sojourn is the wait of the event the router took last; an empty queue
//! reads zero. Ordinary operation holds the queue near empty, a handful of
//! events; a backlog of hundreds, or an event waiting half a second, already
//! means the router lags its traffic. The refusal bounds sit well above
//! ordinary operation, so the worker's own admission ladder and the proxy's
//! per-worker AIMD cap, both driven by the `X-Overload` load signal, act first
//! under ordinary overload; the gate catches a router that falls behind while
//! that signal does not show it (a process starved of CPU). The refusal sojourn
//! stays under the proxy probe's reply window (`timeout_ms`, 1500): a router
//! that lags that much is refused before its answers could be read late.
//!
//! The ladder refuses from the router's first reading past a refusal bound;
//! the proxy reads the refusal on its next probe answer (`interval_ms`, 1000)
//! and places new calls on the other workers, so the INVITEs placed on the
//! worker in between get its `503` and their callers retry. Health, and so
//! every dialog the worker owns, is unaffected. The hold spans at least two probe intervals,
//! so a refusal is read by a probe and the worker does not flap at the probe's
//! cadence; it is not tied to the probe's `threshold` (2), which only counts
//! missed answers. A refusing worker's answers keep their load payload, so the
//! AIMD cap keeps tracking it and resumes from it once the worker accepts.
//!
//! Unset or blank takes the default. A value that is not a positive integer
//! refuses the boot naming its variable, and so do an accept depth not below
//! the refuse depth, a refuse depth past the event queue's bound, and an
//! accept sojourn not below the refuse sojourn. The relation to the probe
//! timers is the deployment's to keep: they are the proxy's settings, which a
//! worker does not read.

use b2bua::config::EventQueueGateConfig;

use crate::stated;

const REFUSE_DEPTH: &str = "B2BUA_EVENT_QUEUE_REFUSE_DEPTH";
const ACCEPT_DEPTH: &str = "B2BUA_EVENT_QUEUE_ACCEPT_DEPTH";
const REFUSE_SOJOURN: &str = "B2BUA_EVENT_QUEUE_REFUSE_SOJOURN_MS";
const ACCEPT_SOJOURN: &str = "B2BUA_EVENT_QUEUE_ACCEPT_SOJOURN_MS";
const ACCEPT_HOLD: &str = "B2BUA_EVENT_QUEUE_ACCEPT_HOLD_MS";

/// The gate's bounds `lookup` states, checked against the event queue's
/// bound `capacity`. `Err` names the variables.
pub(crate) fn event_queue_gate_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    capacity: usize,
) -> Result<EventQueueGateConfig, String> {
    let d = EventQueueGateConfig::default();
    let positive = |key: &str, default: u64| -> Result<u64, String> {
        let Some(v) = stated(lookup(key)) else { return Ok(default) };
        match v.trim().parse::<u64>() {
            Ok(0) => Err(format!("{key}=0: must be a positive integer")),
            Ok(n) => Ok(n),
            Err(e) => Err(format!("{key}={v:?}: {e}")),
        }
    };
    let count = |key: &str, default: usize| -> Result<usize, String> {
        let n = positive(key, u64::try_from(default).unwrap_or(u64::MAX))?;
        usize::try_from(n).map_err(|_| format!("{key}={n}: out of range"))
    };
    let gate = EventQueueGateConfig {
        refuse_depth: count(REFUSE_DEPTH, d.refuse_depth)?,
        accept_depth: count(ACCEPT_DEPTH, d.accept_depth)?,
        refuse_sojourn_ms: positive(REFUSE_SOJOURN, d.refuse_sojourn_ms)?,
        accept_sojourn_ms: positive(ACCEPT_SOJOURN, d.accept_sojourn_ms)?,
        accept_hold_ms: positive(ACCEPT_HOLD, d.accept_hold_ms)?,
    };
    if gate.accept_depth >= gate.refuse_depth {
        return Err(format!(
            "{ACCEPT_DEPTH}={} must be below {REFUSE_DEPTH}={}: the gate would never accept",
            gate.accept_depth, gate.refuse_depth
        ));
    }
    if gate.refuse_depth > capacity {
        return Err(format!(
            "{REFUSE_DEPTH}={} exceeds the event queue's bound {capacity}: the depth could \
             never reach it",
            gate.refuse_depth
        ));
    }
    if gate.accept_sojourn_ms >= gate.refuse_sojourn_ms {
        return Err(format!(
            "{ACCEPT_SOJOURN}={} must be below {REFUSE_SOJOURN}={}: the gate would never accept",
            gate.accept_sojourn_ms, gate.refuse_sojourn_ms
        ));
    }
    Ok(gate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: usize = 4096;

    fn read(pairs: &[(&str, &str)]) -> Result<EventQueueGateConfig, String> {
        event_queue_gate_from_lookup(
            |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()),
            CAPACITY,
        )
    }

    #[test]
    fn unset_or_blank_takes_the_defaults() {
        let defaults = EventQueueGateConfig {
            refuse_depth: 512,
            accept_depth: 64,
            refuse_sojourn_ms: 500,
            accept_sojourn_ms: 50,
            accept_hold_ms: 3_000,
        };
        assert_eq!(read(&[]), Ok(defaults));
        assert_eq!(read(&[(REFUSE_DEPTH, " ")]), Ok(defaults));
        assert_eq!(EventQueueGateConfig::default(), defaults);
    }

    #[test]
    fn each_variable_is_read() {
        let gate = read(&[
            (REFUSE_DEPTH, "1024"),
            (ACCEPT_DEPTH, "128"),
            (REFUSE_SOJOURN, "800"),
            (ACCEPT_SOJOURN, "100"),
            (ACCEPT_HOLD, "5000"),
        ])
        .unwrap();
        assert_eq!(
            gate,
            EventQueueGateConfig {
                refuse_depth: 1024,
                accept_depth: 128,
                refuse_sojourn_ms: 800,
                accept_sojourn_ms: 100,
                accept_hold_ms: 5_000,
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_positive_integer_refuses_the_boot_naming_it() {
        for key in [REFUSE_DEPTH, ACCEPT_DEPTH, REFUSE_SOJOURN, ACCEPT_SOJOURN, ACCEPT_HOLD] {
            for bad in ["0", "-1", "2s"] {
                let err = read(&[(key, bad)]).unwrap_err();
                assert!(err.starts_with(key), "{key}={bad}: {err}");
            }
        }
    }

    #[test]
    fn an_accept_depth_not_below_the_refuse_depth_refuses_the_boot_naming_both() {
        let err = read(&[(REFUSE_DEPTH, "300"), (ACCEPT_DEPTH, "300")]).unwrap_err();
        assert!(err.contains(REFUSE_DEPTH) && err.contains(ACCEPT_DEPTH), "{err}");
    }

    #[test]
    fn a_refuse_depth_past_the_queue_bound_refuses_the_boot_naming_it() {
        assert!(read(&[(REFUSE_DEPTH, "4096")]).is_ok(), "the bound itself is reachable");
        let err = read(&[(REFUSE_DEPTH, "4097")]).unwrap_err();
        assert!(err.contains(REFUSE_DEPTH) && err.contains("4096"), "{err}");
    }

    #[test]
    fn an_accept_sojourn_not_below_the_refuse_sojourn_refuses_the_boot_naming_both() {
        let err = read(&[(REFUSE_SOJOURN, "50")]).unwrap_err();
        assert!(err.contains(REFUSE_SOJOURN) && err.contains(ACCEPT_SOJOURN), "{err}");
    }
}
