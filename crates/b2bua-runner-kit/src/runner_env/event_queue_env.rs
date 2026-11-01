//! The event-queue gate's env grammar (ADR-0050 §9): when a worker whose
//! transaction event queue is backed up refuses new calls, on its OPTIONS
//! answer (`new=refuse; cause=overload`) and at its admission ladder's
//! event-queue rung (ADR-0037 item 11, a `503` to a new normal INVITE), and
//! when it accepts them again. Both read the one gate: one state, one set of
//! bounds, one hold.
//!
//! | variable | role | default |
//! |---|---|---|
//! | `B2BUA_EVENT_QUEUE_REFUSE_DEPTH` | queued events at which new calls are refused, at once | 512 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_DEPTH` | queued events at or below which they may be accepted again | 64 |
//! | `B2BUA_EVENT_QUEUE_REFUSE_DELAY_MS` | the queue's delay, ms, at or past which new calls are refused once it persists | 300 |
//! | `B2BUA_EVENT_QUEUE_REFUSE_INTERVAL_MS` | how long, ms, the delay stays at or past the refusal delay before new calls are refused | 150 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_DELAY_MS` | the queue's delay, ms, at or below which they may be accepted again | 50 |
//! | `B2BUA_EVENT_QUEUE_ACCEPT_HOLD_MS` | how long both accept bounds hold before new calls are accepted again | 3000 |
//!
//! The delay is the age of the oldest event waiting in the queue, read when
//! the gate reads: a router that takes nothing still reads it grow. Ordinary
//! operation holds the queue near empty, a handful of events; a backlog of
//! hundreds already means the router lags its traffic, so the depth refuses
//! at once. The refusal delay sits below SIP's T1 (500 ms), so the worker
//! refuses before its peers start retransmitting what waits in its queue,
//! and well above the gap a CPU quota throttles a process for (about 70 ms of
//! each 100 ms period). The interval is CoDel's persistence and spans more
//! than one quota period: a throttle gap, or a slow turn shorter than the
//! refusal delay plus the interval (450 ms), trips nothing; a single turn at
//! least that long trips it on the next reading. While the gate is read, at
//! each event the router takes and each probe or new INVITE, 450 ms is the
//! longest one event waits before the worker refuses, under T1; a router held
//! in one turn reads nothing until that turn ends. The refusal bounds sit well
//! above ordinary operation, so the worker's other admission rungs and the
//! proxy's per-worker AIMD cap, driven by the `X-Overload` load signal, act
//! first under ordinary overload; the gate catches a router that falls
//! behind while that signal does not show it (a process starved of CPU).
//!
//! The ladder refuses from the router's first reading that trips the gate;
//! the proxy reads the refusal on its next probe answer (`interval_ms`, 1000)
//! and places new calls on the other workers, so the INVITEs placed on the
//! worker in between get its `503` and their callers retry. Health, and so
//! every dialog the worker owns, is unaffected. The refusal delay plus the
//! interval stays under the probe's reply window (`timeout_ms`, 1500): a
//! router that lags that much is refused before its answers could be read
//! late. The hold spans at least two probe intervals, so a refusal is read by
//! a probe and the worker does not flap at the probe's cadence; it is not
//! tied to the probe's `threshold` (2), which only counts missed answers. A
//! refusing worker's answers keep their load payload, so the AIMD cap keeps
//! tracking it and resumes from it once the worker accepts.
//!
//! Unset or blank takes the default. A value that is not a positive integer
//! refuses the boot naming its variable, and so does each broken relation,
//! naming both: an accept depth not below the refuse depth, a refuse depth
//! past the event queue's bound, an accept delay not below the refuse delay,
//! and a refusal interval not below the hold (a refusal would end before a
//! delay that persists could trip it again). The relation to the probe timers
//! is the deployment's to keep: they are the proxy's settings, which a worker
//! does not read.

use b2bua::config::{EventQueueGateConfig, GateBound, GateViolation, EVENT_QUEUE_CAPACITY};

use crate::stated;

const REFUSE_DEPTH: &str = "B2BUA_EVENT_QUEUE_REFUSE_DEPTH";
const ACCEPT_DEPTH: &str = "B2BUA_EVENT_QUEUE_ACCEPT_DEPTH";
const REFUSE_DELAY: &str = "B2BUA_EVENT_QUEUE_REFUSE_DELAY_MS";
const REFUSE_INTERVAL: &str = "B2BUA_EVENT_QUEUE_REFUSE_INTERVAL_MS";
const ACCEPT_DELAY: &str = "B2BUA_EVENT_QUEUE_ACCEPT_DELAY_MS";
const ACCEPT_HOLD: &str = "B2BUA_EVENT_QUEUE_ACCEPT_HOLD_MS";

/// The gate's bounds `lookup` states, checked against each other and the
/// event queue's bound ([`EventQueueGateConfig::validate`]). `Err` names the
/// variables.
pub(crate) fn event_queue_gate_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
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
        refuse_delay_ms: positive(REFUSE_DELAY, d.refuse_delay_ms)?,
        refuse_interval_ms: positive(REFUSE_INTERVAL, d.refuse_interval_ms)?,
        accept_delay_ms: positive(ACCEPT_DELAY, d.accept_delay_ms)?,
        accept_hold_ms: positive(ACCEPT_HOLD, d.accept_hold_ms)?,
    };
    let named = |b: GateBound| match b {
        GateBound::RefuseDepth => (REFUSE_DEPTH, gate.refuse_depth.to_string()),
        GateBound::AcceptDepth => (ACCEPT_DEPTH, gate.accept_depth.to_string()),
        GateBound::RefuseDelay => (REFUSE_DELAY, gate.refuse_delay_ms.to_string()),
        GateBound::RefuseInterval => (REFUSE_INTERVAL, gate.refuse_interval_ms.to_string()),
        GateBound::AcceptDelay => (ACCEPT_DELAY, gate.accept_delay_ms.to_string()),
        GateBound::AcceptHold => (ACCEPT_HOLD, gate.accept_hold_ms.to_string()),
    };
    gate.validate().map_err(|violation| match violation {
        GateViolation::Zero(b) => format!("{}=0: must be a positive integer", named(b).0),
        GateViolation::NotBelow { low, high, why } => {
            let ((low, l), (high, h)) = (named(low), named(high));
            format!("{low}={l} must be below {high}={h}: {why}")
        }
        GateViolation::PastTheQueue => format!(
            "{REFUSE_DEPTH}={} exceeds the event queue's bound {EVENT_QUEUE_CAPACITY}: the \
             depth could never reach it",
            gate.refuse_depth
        ),
    })?;
    Ok(gate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<EventQueueGateConfig, String> {
        event_queue_gate_from_lookup(|key| {
            pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn unset_or_blank_takes_the_defaults() {
        let defaults = EventQueueGateConfig {
            refuse_depth: 512,
            accept_depth: 64,
            refuse_delay_ms: 300,
            refuse_interval_ms: 150,
            accept_delay_ms: 50,
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
            (REFUSE_DELAY, "400"),
            (REFUSE_INTERVAL, "200"),
            (ACCEPT_DELAY, "100"),
            (ACCEPT_HOLD, "5000"),
        ])
        .unwrap();
        assert_eq!(
            gate,
            EventQueueGateConfig {
                refuse_depth: 1024,
                accept_depth: 128,
                refuse_delay_ms: 400,
                refuse_interval_ms: 200,
                accept_delay_ms: 100,
                accept_hold_ms: 5_000,
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_positive_integer_refuses_the_boot_naming_it() {
        let keys =
            [REFUSE_DEPTH, ACCEPT_DEPTH, REFUSE_DELAY, REFUSE_INTERVAL, ACCEPT_DELAY, ACCEPT_HOLD];
        for key in keys {
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
    fn an_accept_delay_not_below_the_refuse_delay_refuses_the_boot_naming_both() {
        let err = read(&[(REFUSE_DELAY, "50")]).unwrap_err();
        assert!(err.contains(REFUSE_DELAY) && err.contains(ACCEPT_DELAY), "{err}");
    }

    #[test]
    fn a_refusal_interval_not_below_the_hold_refuses_the_boot_naming_both() {
        assert!(read(&[(REFUSE_INTERVAL, "2999")]).is_ok());
        let err = read(&[(REFUSE_INTERVAL, "3000")]).unwrap_err();
        assert!(err.contains(REFUSE_INTERVAL) && err.contains(ACCEPT_HOLD), "{err}");
    }
}
