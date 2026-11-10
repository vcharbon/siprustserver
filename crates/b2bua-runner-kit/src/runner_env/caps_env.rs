//! The per-call caps' env grammar and their boot refusals (ADR-0057 §8). Each
//! `B2BUA_CAP_*` variable left unset takes its upstream default
//! ([`CallCaps::DEFAULT`]); the caps on what grows with a call's age (legs,
//! attempts, CDR events) take a rate per hour of the call's granted duration
//! under `<variable>_PER_HOUR`.
//!
//! Startup refuses:
//! - a cap at zero: no cap can be switched off, only raised;
//! - `B2BUA_CAP_ATTEMPTS` (base or rate) below `B2BUA_CAP_LEGS` times the
//!   failover chain (`MAX_LIMITER_FAILOVER + 1`): every leg may follow refused
//!   admissions;
//! - `B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS` above a held request's answer
//!   deadline less the answer margin: a service's answer would come after the
//!   request it holds was answered;
//! - `B2BUA_CAP_ACTIONS_PER_TURN` at or below `B2BUA_CAP_LEGS` plus the
//!   teardown's two further actions: a call at its base leg cap could not be
//!   torn down.

use b2bua::config::{AgeCap, CallCaps, Cap};

use crate::stated;

const SERVICE_HTTP_TIMEOUT: &str = "B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS";

/// The actions a teardown takes beside one per unresolved leg.
const TEARDOWN_ACTIONS: u64 = 2;

/// The caps `lookup` states over the defaults. An unparsable value is an
/// `Err` naming it.
pub(crate) fn caps_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<CallCaps, String> {
    let d = CallCaps::DEFAULT;
    let count = |key: &str, default: u64| -> Result<u64, String> {
        stated(lookup(key))
            .map(|v| v.trim().parse::<u64>().map_err(|e| format!("{key}={v:?}: {e}")))
            .transpose()
            .map(|v| v.unwrap_or(default))
    };
    let aged = |cap: Cap, default: AgeCap| -> Result<AgeCap, String> {
        let base = count(cap.variable(), default.base)?;
        let per_hour = count(&format!("{}_PER_HOUR", cap.variable()), default.per_hour)?;
        Ok(AgeCap { base, per_hour })
    };
    Ok(CallCaps {
        legs: aged(Cap::Legs, d.legs)?,
        attempts: aged(Cap::Attempts, d.attempts)?,
        cdr_events: aged(Cap::CdrEvents, d.cdr_events)?,
        timers: count(Cap::Timers.variable(), d.timers)?,
        requests: count(Cap::Requests.variable(), d.requests)?,
        dialogs: count(Cap::Dialogs.variable(), d.dialogs)?,
        header_updates: count(Cap::HeaderUpdates.variable(), d.header_updates)?,
        ext_bytes: count(Cap::ExtBytes.variable(), d.ext_bytes)?,
        actions_per_turn: count(Cap::ActionsPerTurn.variable(), d.actions_per_turn)?,
        service_http_timeout_ms: count(SERVICE_HTTP_TIMEOUT, d.service_http_timeout_ms)?,
    })
}

/// The longest a service's HTTP request may run: a held request's answer
/// deadline less the answer margin (ADR-0039).
fn longest_service_http_ms() -> u64 {
    let held = u64::try_from(call::RequestRecord::HELD_ANSWER_MS).unwrap_or(0);
    let margin = u64::try_from(b2bua::answer_deadline::MARGIN.as_millis()).unwrap_or(u64::MAX);
    held.saturating_sub(margin)
}

/// `Err` naming every relation `caps` breaks.
pub(crate) fn check_caps(caps: &CallCaps) -> Result<(), String> {
    let mut out = Vec::new();
    let mut zero = |name: String, value: u64| {
        if value == 0 {
            out.push(format!("{name}=0: a cap cannot be switched off; raise it instead"));
        }
    };
    // A call granted nothing yet is held to each cap's base.
    for cap in Cap::ALL {
        zero(cap.variable().into(), caps.limit(cap, 0));
    }
    for (cap, aged) in
        [(Cap::Legs, caps.legs), (Cap::Attempts, caps.attempts), (Cap::CdrEvents, caps.cdr_events)]
    {
        zero(format!("{}_PER_HOUR", cap.variable()), aged.per_hour);
    }
    zero(SERVICE_HTTP_TIMEOUT.into(), caps.service_http_timeout_ms);
    let chain = u64::from(b2bua::answer_deadline::FAILURE_CHAIN);
    let (legs, attempts) = (caps.legs, caps.attempts);
    for (part, l, a) in
        [("", legs.base, attempts.base), ("_PER_HOUR", legs.per_hour, attempts.per_hour)]
    {
        if a < l.saturating_mul(chain) {
            out.push(format!(
                "B2BUA_CAP_ATTEMPTS{part}={a} < B2BUA_CAP_LEGS{part}={l} × {chain}: every leg may \
                 follow its failover chain's refused admissions"
            ));
        }
    }
    let (fuel, legs) = (caps.actions_per_turn, caps.legs.base);
    if fuel <= legs.saturating_add(TEARDOWN_ACTIONS) {
        out.push(format!(
            "B2BUA_CAP_ACTIONS_PER_TURN={fuel} <= B2BUA_CAP_LEGS={legs} + {TEARDOWN_ACTIONS}: the \
             teardown of a call at its leg cap, one action per unresolved leg and two more, \
             would trip its own fuel"
        ));
    }
    let longest = longest_service_http_ms();
    if caps.service_http_timeout_ms > longest {
        out.push(format!(
            "{SERVICE_HTTP_TIMEOUT}={} > {longest}: a service's answer would come after the \
             request it holds was answered (its deadline less the answer margin)",
            caps.service_http_timeout_ms
        ));
    }
    if out.is_empty() {
        Ok(())
    } else {
        Err(out.join("; "))
    }
}

#[cfg(test)]
mod caps_env_tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn the_defaults_hold_every_relation() {
        assert_eq!(caps_from_lookup(lookup(&[])), Ok(CallCaps::DEFAULT));
        assert_eq!(check_caps(&CallCaps::DEFAULT), Ok(()));
    }

    #[test]
    fn a_zero_cap_and_too_few_attempts_are_refused() {
        let caps = caps_from_lookup(lookup(&[
            ("B2BUA_CAP_TIMERS", "0"),
            ("B2BUA_CAP_LEGS", "3000"),
            ("B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS", "30000"),
        ]))
        .expect("every value parses");
        let err = check_caps(&caps).unwrap_err();
        assert!(err.contains("B2BUA_CAP_TIMERS=0"), "{err}");
        assert!(err.contains("B2BUA_CAP_ATTEMPTS=12000 < B2BUA_CAP_LEGS=3000"), "{err}");
        assert!(err.contains("B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS=30000 > 26000"), "{err}");
    }

    #[test]
    fn a_fuel_that_cannot_tear_down_a_call_at_its_leg_cap_is_refused() {
        let caps = caps_from_lookup(lookup(&[
            ("B2BUA_CAP_ACTIONS_PER_TURN", "1202"),
            ("B2BUA_CAP_LEGS", "1200"),
        ]))
        .expect("every value parses");
        let err = check_caps(&caps).unwrap_err();
        assert!(
            err.contains("B2BUA_CAP_ACTIONS_PER_TURN=1202 <= B2BUA_CAP_LEGS=1200 + 2"),
            "{err}"
        );
        let caps = CallCaps { actions_per_turn: 1203, ..caps };
        assert_eq!(check_caps(&caps), Ok(()));
    }

    #[test]
    fn an_unparsable_cap_names_its_variable() {
        let err = caps_from_lookup(lookup(&[("B2BUA_CAP_LEGS_PER_HOUR", "x")])).unwrap_err();
        assert!(err.starts_with("B2BUA_CAP_LEGS_PER_HOUR"), "{err}");
    }
}
