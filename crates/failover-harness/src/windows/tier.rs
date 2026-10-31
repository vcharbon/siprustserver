//! A cell's tier, measured (ADR-0055 §11): `Off` at level `off`; at level
//! `applied`, `Guaranteed` when a kill struck a cluster every call of which
//! was backed and bootstrapped at every turn up to the strike, no entry of
//! any of them hit the cap, the transaction layer of the node the kill struck
//! sent at its bound, before the strike, nothing it held for a turn of any
//! call on that node but the ones whose delete a backup applied, and the
//! test tampered with no replication stream or replica; `Degraded`
//! otherwise.

use b2bua::repl::SyncLevel;

use super::{Strike, Tier};
use crate::incident::Incident;
use crate::turn_points::{BoundPass, Turn};

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
    /// Every node whose transaction layer sent messages held for a turn of a
    /// call at their bound, before that turn's write, up to the strike, with
    /// the call.
    pub bound_passes: &'a [BoundPass],
}

/// The tier `m` places a cell in.
pub fn measure(m: &Measured<'_>) -> Tier {
    if m.level == SyncLevel::Off {
        return Tier::Off;
    }
    let killed = m.strike.incident().is_some_and(|i| i.kills());
    let backed = m.at_strike.iter().all(|(_, turns)| turns.iter().all(|t| t.backed));
    let placed = m.at_strike.first().and_then(|(c, _)| call::call_ref_primary(c));
    let bound_passed = m.bound_passes.iter().any(|p| struck(m.strike, placed, &p.node));
    let capped = bound_passed || m.at_end.iter().any(|(_, turns)| turns.iter().any(Turn::cap_hit));
    if killed && backed && !capped && !m.tampered {
        Tier::Guaranteed
    } else {
        Tier::Degraded
    }
}

/// Whether `node` is one `strike` kills: the primary of the call the test
/// placed first, `placed`, for a kill of the primary, any other node for a
/// kill of its backup. Held messages a surviving node sent at their bound
/// lose nothing.
fn struck(strike: Strike, placed: Option<&str>, node: &str) -> bool {
    let Some(placed) = placed else { return true };
    match strike.incident() {
        Some(incident) if incident.kills_the_primary() => node == placed,
        Some(Incident::BackupKilled) => node != placed,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        vec![("b1|c1|f1".to_string(), turns)]
    }

    fn tier(
        level: SyncLevel,
        incident: Incident,
        at: &[(String, Vec<Turn>)],
        tampered: bool,
    ) -> Tier {
        let strike = Strike::from(incident);
        measure(&Measured { level, strike, at_strike: at, at_end: at, tampered, bound_passes: &[] })
    }

    fn pass(node: &str, call_ref: &str) -> BoundPass {
        BoundPass { node: node.to_string(), call_ref: Some(call_ref.to_string()) }
    }

    fn tier_after(incident: Incident, passes: &[BoundPass]) -> Tier {
        let at = calls(vec![turn(true, Some(Released::Applied), false)]);
        measure(&Measured {
            level: SyncLevel::Applied,
            strike: Strike::from(incident),
            at_strike: &at,
            at_end: &at,
            tampered: false,
            bound_passes: passes,
        })
    }

    #[test]
    fn a_bound_pass_on_the_node_the_kill_strikes_is_degraded() {
        let on_primary = [pass("b1", "b1|c2|f2")];
        let on_backup = [pass("b2", "b1|c2|f2")];
        for incident in
            [Incident::PrimaryKilled, Incident::PrimaryKilledUnannounced, Incident::PrimaryRebooted]
        {
            assert_eq!(tier_after(incident, &on_primary), Tier::Degraded, "{incident:?}");
        }
        assert_eq!(tier_after(Incident::BackupKilled, &on_backup), Tier::Degraded);
    }

    #[test]
    fn a_bound_pass_on_a_node_the_kill_spares_keeps_it_guaranteed() {
        let on_primary = [pass("b1", "b1|c1|f1")];
        // The backup's copy of a call it took over sent at the bound.
        let on_backup = [pass("b2", "b1|c1|f1")];
        // A call the other worker is primary of, whose backup the kill spares.
        let elsewhere = [pass("b2", "b2|c9|f9")];
        assert_eq!(tier_after(Incident::PrimaryKilled, &on_backup), Tier::Guaranteed);
        assert_eq!(tier_after(Incident::PrimaryRebooted, &elsewhere), Tier::Guaranteed);
        assert_eq!(tier_after(Incident::BackupKilled, &on_primary), Tier::Guaranteed);
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
