//! A cell's tier, measured (ADR-0055 §11): `Off` at level `off`; at level
//! `applied`, `Guaranteed` when a kill struck a cluster every call of which
//! was backed and bootstrapped at every turn up to the strike, no entry of
//! any of them hit the cap, and the test tampered with no replication stream
//! or replica; `Degraded` otherwise.

use b2bua::repl::SyncLevel;

use super::{Strike, Tier};
use crate::turn_points::Turn;

/// What a tier is measured from.
pub struct Measured<'a> {
    pub level: SyncLevel,
    pub strike: Strike,
    /// Every call's turns at its primary when the incident struck.
    pub at_strike: &'a [(String, Vec<Turn>)],
    /// Every call's turns at its primary at the end of the run.
    pub at_end: &'a [(String, Vec<Turn>)],
    /// The test stalled, delayed or cut a replication stream, or rewrote a
    /// replica, before or during the run: what a backup holds is not what
    /// its turns report.
    pub tampered: bool,
}

/// The tier `m` places a cell in.
pub fn measure(m: &Measured<'_>) -> Tier {
    if m.level == SyncLevel::Off {
        return Tier::Off;
    }
    let killed = m.strike.incident().is_some_and(|i| i.kills());
    let backed = m.at_strike.iter().all(|(_, turns)| turns.iter().all(|t| t.backed));
    let capped = m.at_end.iter().any(|(_, turns)| turns.iter().any(Turn::cap_hit));
    if killed && backed && !capped && !m.tampered {
        Tier::Guaranteed
    } else {
        Tier::Degraded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::incident::Incident;
    use crate::turn_points::Released;

    fn turn(backed: bool, released: Option<Released>, capped_by_harness: bool) -> Turn {
        Turn {
            version: (1, 0),
            wrote: true,
            on_request: true,
            on_answer: false,
            called_out: false,
            released,
            capped_by_harness,
            applied: true,
            backed,
        }
    }

    fn calls(turns: Vec<Turn>) -> Vec<(String, Vec<Turn>)> {
        vec![("b1~c1".to_string(), turns)]
    }

    fn tier(
        level: SyncLevel,
        incident: Incident,
        at: &[(String, Vec<Turn>)],
        tampered: bool,
    ) -> Tier {
        let strike = Strike::from(incident);
        measure(&Measured { level, strike, at_strike: at, at_end: at, tampered })
    }

    #[test]
    fn a_backed_kill_without_a_cap_hit_is_guaranteed() {
        let at = calls(vec![turn(true, Some(Released::Applied), false)]);
        assert_eq!(tier(SyncLevel::Applied, Incident::PrimaryKilled, &at, false), Tier::Guaranteed);
        assert_eq!(tier(SyncLevel::Applied, Incident::BackupKilled, &at, false), Tier::Guaranteed);
    }

    #[test]
    fn the_harness_hold_and_a_lost_backup_are_no_cap_hit() {
        let at = calls(vec![
            turn(true, Some(Released::Capped), true),
            turn(true, Some(Released::BackupLost), false),
        ]);
        assert_eq!(tier(SyncLevel::Applied, Incident::PrimaryKilled, &at, false), Tier::Guaranteed);
    }

    #[test]
    fn an_unbacked_turn_a_cap_hit_or_a_tampered_stream_is_degraded() {
        let unbacked = calls(vec![turn(false, Some(Released::AtOnce), false)]);
        let capped = calls(vec![turn(true, Some(Released::Capped), false)]);
        let fine = calls(vec![turn(true, Some(Released::Applied), false)]);
        let at = SyncLevel::Applied;
        assert_eq!(tier(at, Incident::PrimaryKilled, &unbacked, false), Tier::Degraded);
        assert_eq!(tier(at, Incident::PrimaryKilled, &capped, false), Tier::Degraded);
        assert_eq!(tier(at, Incident::PrimaryKilled, &fine, true), Tier::Degraded);
    }

    #[test]
    fn only_a_kill_is_guaranteed_and_off_is_off() {
        let fine = calls(vec![turn(true, Some(Released::Applied), false)]);
        for incident in [Incident::Split, Incident::SplitHealed, Incident::StalledKilled] {
            assert_eq!(tier(SyncLevel::Applied, incident, &fine, false), Tier::Degraded);
        }
        assert_eq!(tier(SyncLevel::Off, Incident::PrimaryKilled, &fine, false), Tier::Off);
    }
}
