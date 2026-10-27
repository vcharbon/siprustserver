//! The rows of the window table, in the order they take a finding. Each `W-…`
//! id is the backticked id of its ADR-0055 §11 bullet.

use scenario_harness::dialogs::DIALOG_ENDED;

use super::copies::{
    cancels_an_answered_invite, non_2xx_after_another_final, non_2xx_after_the_dead_owners_2xx,
    second_bye_at_one_cseq, RULE_EARLY_BYE, RULE_TAG_CONSISTENCY, RULE_UNACKED_NON_2XX,
};
use super::stall_cseq::reuses_a_stall_cseq;
use super::stall_repeat::{reason_only_cancel_repeat, RULE_RUNG_BYTE_IDENTICAL};
use super::unseen_rseq::{from_another_node, RULES as RSEQ_RULES};
use super::{outcome, Kind, Opens, Row, Tier, Transitional, Witness};
use crate::incident::Incident;
use crate::incident::Incident::{
    BackupKilled, PrimaryKilled, Split, SplitHealed, StalledKilled, UnbackedKilled,
};
use crate::turn_points::TurnPoint;
use crate::{RULE_CSEQ_IN_DIALOG_ORDER, RULE_NO_CANCEL_AFTER_FINAL};
use b2bua::repl::SyncLevel;

/// The rows of a split, healed or not, and of a stall past the cap then a
/// kill: two acting copies.
const TWO_COPIES: &[Incident] = &[Split, SplitHealed, StalledKilled];
/// The rows of a split, healed or not.
const SPLITS: &[Incident] = &[Split, SplitHealed];

/// The cell `shape`/`level`/`incident`/t`turn`-`point`.
const fn witness(
    shape: &'static str,
    level: SyncLevel,
    incident: Incident,
    turn: usize,
    point: TurnPoint,
) -> Option<Witness> {
    Some(Witness { shape, level, incident, turn, point })
}

/// Every row, in the order they take a finding.
pub static TABLE: &[Row] = &[
    // A split, healed or not, and a stall past the cap then a kill.
    Row {
        id: "W-split-ended-dialog",
        tier: Tier::Degraded,
        opens: Opens::any(TWO_COPIES),
        covers: "the stale copy's requests into a dialog the other copy ended, each closed by the \
                 481 it draws",
        kind: Kind::Dialog { rule: DIALOG_ENDED, methods: &[], once_per_dialog: false },
        closes_when_over: false,
        witness: witness("basic", SyncLevel::Applied, SplitHealed, 5, TurnPoint::Logged),
    },
    Row {
        id: "W-split-cancel-answered",
        tier: Tier::Degraded,
        opens: Opens::any(TWO_COPIES),
        covers: "one copy's CANCEL of an INVITE the other copy saw answered, and its repeats \
                 after the heal",
        kind: Kind::Rfc {
            rules: &[RULE_NO_CANCEL_AFTER_FINAL],
            once_per_dialog: true,
            admits: Some(|i, capture, _| cancels_an_answered_invite(i, capture, u64::MAX)),
        },
        closes_when_over: false,
        witness: witness("info-x-bye", SyncLevel::Applied, StalledKilled, 3, TurnPoint::Logged),
    },
    Row {
        id: "W-split-non-2xx-after-final",
        tier: Tier::Degraded,
        opens: Opens::any(TWO_COPIES),
        covers: "one copy's non-2xx after the final the other copy sent the caller, under \
                 whatever To-tag, which she discards, and its repeats",
        kind: Kind::Rfc {
            rules: &[RULE_UNACKED_NON_2XX, RULE_TAG_CONSISTENCY],
            once_per_dialog: false,
            admits: Some(|i, capture, _| non_2xx_after_another_final(i, capture)),
        },
        closes_when_over: false,
        witness: witness("refer", SyncLevel::Applied, Split, 1, TurnPoint::Applied),
    },
    Row {
        id: "W-split-early-bye",
        tier: Tier::Degraded,
        opens: Opens::any(TWO_COPIES),
        covers: "one copy's BYE of a caller dialog it holds early, which the other copy answered",
        kind: Kind::Rfc { rules: &[RULE_EARLY_BYE], once_per_dialog: true, admits: None },
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "W-split-two-byes",
        tier: Tier::Degraded,
        opens: Opens::any(SPLITS),
        covers: "two BYEs of one dialog at one CSeq, one from each copy",
        kind: Kind::Rfc {
            rules: &[RULE_CSEQ_IN_DIALOG_ORDER],
            once_per_dialog: false,
            admits: Some(|i, capture, _| second_bye_at_one_cseq(i, capture)),
        },
        closes_when_over: false,
        witness: witness("keepalive", SyncLevel::Applied, SplitHealed, 4, TurnPoint::Logged),
    },
    Row {
        id: "W-split-cseq",
        tier: Tier::Degraded,
        opens: Opens::any(SPLITS),
        covers: "a CSeq one copy re-uses after the other sent it",
        kind: Kind::Rfc {
            rules: &[RULE_CSEQ_IN_DIALOG_ORDER],
            once_per_dialog: false,
            admits: None,
        },
        closes_when_over: false,
        witness: witness("keepalive", SyncLevel::Applied, SplitHealed, 14, TurnPoint::Logged),
    },
    Row {
        id: "W-split-outcome",
        tier: Tier::Degraded,
        opens: Opens::any(SPLITS),
        covers: "any party verdict: a split is judged on leaks, cleanup and the CDR count alone",
        kind: Kind::Outcome(outcome::any),
        closes_when_over: false,
        witness: witness("media-leg", SyncLevel::Applied, SplitHealed, 1, TurnPoint::Logged),
    },
    Row {
        id: "W-cdr-split",
        tier: Tier::Degraded,
        opens: Opens::any(&[Split]),
        covers: "no CDR or two: each copy of a split that never heals writes its own or none",
        kind: Kind::Cdr { min: 0, max: 2 },
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "W-cdr-two",
        tier: Tier::Degraded,
        opens: Opens::any(&[SplitHealed, StalledKilled]),
        covers: "two CDRs: each copy of a healed split, or the stalled primary and the departure \
                 sweep, writes one",
        kind: Kind::Cdr { min: 1, max: 2 },
        closes_when_over: false,
        witness: witness("basic", SyncLevel::Applied, StalledKilled, 5, TurnPoint::Logged),
    },
    // A stall past the cap then a kill.
    Row {
        id: "W-stall-outcome",
        tier: Tier::Degraded,
        opens: Opens::any(&[StalledKilled]),
        covers: "any party verdict, every request that reached a living worker having drawn its \
                 final there",
        kind: Kind::Outcome(outcome::living_answered),
        closes_when_over: false,
        witness: witness("media-leg", SyncLevel::Applied, StalledKilled, 1, TurnPoint::Left),
    },
    Row {
        id: "W-stall-cancel-reason",
        tier: Tier::Degraded,
        opens: Opens::any(&[StalledKilled]),
        covers:
            "a copy's repeat of a CANCEL that differs from the dead owner's only in its Reason \
                 header field",
        kind: Kind::Rfc {
            rules: &[RULE_RUNG_BYTE_IDENTICAL],
            once_per_dialog: false,
            admits: Some(|i, capture, _| reason_only_cancel_repeat(i, capture)),
        },
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "W-stall-cseq",
        tier: Tier::Degraded,
        opens: Opens::any(&[StalledKilled]),
        covers: "a copy's requests at the CSeqs the dead owner sent on the dialog during the \
                 stall, each closed by the 481 it draws",
        kind: Kind::Rfc {
            rules: &[RULE_CSEQ_IN_DIALOG_ORDER],
            once_per_dialog: false,
            admits: Some(reuses_a_stall_cseq),
        },
        closes_when_over: false,
        witness: witness("prack", SyncLevel::Applied, StalledKilled, 2, TurnPoint::Left),
    },
    // A call with no backup, killed with its primary.
    Row {
        id: "W-unbacked-lost",
        tier: Tier::Degraded,
        opens: Opens::any(&[UnbackedKilled]),
        covers: "every dialog, the caller's included, left to its endpoint's own timers, every \
                 request that reached a living worker having drawn its final there",
        kind: Kind::Outcome(outcome::lost_like_a_non_ha_call),
        closes_when_over: false,
        witness: witness("basic", SyncLevel::Applied, UnbackedKilled, 1, TurnPoint::Logged),
    },
    Row {
        id: "W-cdr-unbacked",
        tier: Tier::Degraded,
        opens: Opens::any(&[UnbackedKilled]),
        covers: "no CDR: a call with no backup is lost with its primary",
        kind: Kind::Cdr { min: 0, max: 1 },
        closes_when_over: false,
        witness: witness("basic", SyncLevel::Applied, UnbackedKilled, 3, TurnPoint::Left),
    },
    // Every incident that keeps the parties' verdict.
    Row {
        id: "W-clean-end",
        tier: Tier::Degraded,
        opens: Opens::any(&[PrimaryKilled, BackupKilled, StalledKilled, UnbackedKilled]),
        covers: "a party the system under test ends cleanly drops her remaining goals",
        kind: Kind::GoalDrop,
        closes_when_over: false,
        witness: witness("basic", SyncLevel::Applied, StalledKilled, 1, TurnPoint::Logged),
    },
    // A kill of the primary.
    Row {
        id: "W-kill-cancel-answered",
        tier: Tier::Degraded,
        opens: Opens::Incidents { always: &[UnbackedKilled], in_window: &[PrimaryKilled] },
        covers: "a copy's one CANCEL of a callee's INVITE the dead owner saw finally answered",
        kind: Kind::Rfc {
            rules: &[RULE_NO_CANCEL_AFTER_FINAL],
            once_per_dialog: true,
            admits: Some(cancels_an_answered_invite),
        },
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "W-clean-end-481",
        tier: Tier::Degraded,
        opens: Opens::Incidents {
            always: &[UnbackedKilled, StalledKilled],
            in_window: &[PrimaryKilled],
        },
        covers: "the clean end's BYE, or terminating NOTIFY, into a dialog the other owner \
                 already ended, closed by its 481, once per dialog",
        kind: Kind::Dialog {
            rule: DIALOG_ENDED,
            methods: &["BYE", "NOTIFY"],
            once_per_dialog: true,
        },
        closes_when_over: false,
        witness: witness("refer", SyncLevel::Off, PrimaryKilled, 13, TurnPoint::Left),
    },
    Row {
        id: "W-kill-teardown-lost",
        tier: Tier::Degraded,
        opens: Opens::any(&[PrimaryKilled]),
        covers: "a dialog left to its endpoint's own timer when the write of a teardown the dead \
                 primary began was not applied",
        kind: Kind::Outcome(outcome::teardown_lost),
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "W-kill-effects-lost",
        tier: Tier::Degraded,
        opens: Opens::any(&[PrimaryKilled]),
        covers: "the effects of a turn taken on something no peer repeats, lost with the node: \
                 every dialog left to its endpoint's own timers",
        kind: Kind::Outcome(outcome::effects_lost),
        closes_when_over: false,
        witness: witness("media-leg", SyncLevel::Off, PrimaryKilled, 2, TurnPoint::Left),
    },
    Row {
        id: "W-kill-answer-lost",
        tier: Tier::Degraded,
        opens: Opens::any(&[PrimaryKilled]),
        covers: "an outside service's answer lost with the dead node: the copy stays reactive and \
                 every dialog is left to its endpoint's own timers",
        kind: Kind::Outcome(outcome::answer_lost),
        closes_when_over: false,
        witness: witness("refer-x-bye", SyncLevel::Off, PrimaryKilled, 5, TurnPoint::Applied),
    },
    Row {
        id: "W-kill-cseq",
        tier: Tier::Degraded,
        opens: Opens::Incidents { always: &[UnbackedKilled], in_window: &[PrimaryKilled] },
        covers: "a CSeq the dead primary sent before the backup applied the write, re-used once \
                 per dialog by a node without its record",
        kind: Kind::Rfc {
            rules: &[RULE_CSEQ_IN_DIALOG_ORDER],
            once_per_dialog: true,
            admits: None,
        },
        closes_when_over: false,
        witness: witness("prack", SyncLevel::Applied, UnbackedKilled, 3, TurnPoint::Left),
    },
    Row {
        id: "W-kill-rseq",
        tier: Tier::Degraded,
        opens: Opens::Incidents { always: &[SplitHealed], in_window: &[PrimaryKilled] },
        covers: "a copy's reliable provisional under the dialog's derived tag whose RSeq cannot \
                 continue the dead primary's, once per dialog",
        kind: Kind::Rfc {
            rules: &RSEQ_RULES,
            once_per_dialog: true,
            admits: Some(|i, capture, _| from_another_node(i, capture)),
        },
        closes_when_over: false,
        witness: witness("prack", SyncLevel::Applied, SplitHealed, 2, TurnPoint::Logged),
    },
    Row {
        id: "W-kill-non-2xx-after-2xx",
        tier: Tier::Degraded,
        opens: Opens::in_window(&[PrimaryKilled]),
        covers: "a copy's non-2xx to a caller's INVITE the dead primary answered 2xx, which she \
                 discards",
        kind: Kind::Rfc {
            rules: &[RULE_UNACKED_NON_2XX],
            once_per_dialog: false,
            admits: Some(non_2xx_after_the_dead_owners_2xx),
        },
        closes_when_over: false,
        witness: None,
    },
    // Hand-written tests' faults no incident names.
    Row {
        id: "T-lag-copy-acts",
        tier: Tier::Degraded,
        opens: Opens::Transitional(Transitional::AnsweredInsideTheLag),
        covers: "the copy CANCELs the callee after its answer and refuses the caller after her \
                 200, acting on a call it never saw answered",
        kind: Kind::Rfc {
            rules: &[RULE_NO_CANCEL_AFTER_FINAL, RULE_UNACKED_NON_2XX],
            once_per_dialog: false,
            admits: None,
        },
        closes_when_over: true,
        witness: None,
    },
    Row {
        id: "T-lag-clean-end-481",
        tier: Tier::Degraded,
        opens: Opens::Transitional(Transitional::AnsweredInsideTheLag),
        covers: "the clean end's BYE, or terminating NOTIFY, into a dialog the other owner \
                 already ended, closed by its 481, once per dialog",
        kind: Kind::Dialog {
            rule: DIALOG_ENDED,
            methods: &["BYE", "NOTIFY"],
            once_per_dialog: true,
        },
        closes_when_over: false,
        witness: None,
    },
    Row {
        id: "T-departure-unacked-final",
        tier: Tier::Degraded,
        opens: Opens::Transitional(Transitional::UndecidedDeparture),
        covers: "a final no node ACKs: the callee's give-up after its node departed, or the \
                 backup's refusal after the caller's transaction completed",
        kind: Kind::Rfc { rules: &[RULE_UNACKED_NON_2XX], once_per_dialog: false, admits: None },
        closes_when_over: false,
        witness: None,
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The rows no upstream sweep cell fires today, at either level: kept, as
    /// Degraded keeps every §11 window, until a shape or incident exercises them.
    const UNWITNESSED: &[&str] = &[
        "W-cdr-split",
        "W-kill-cancel-answered",
        "W-kill-non-2xx-after-2xx",
        "W-kill-teardown-lost",
        "W-split-early-bye",
        "W-stall-cancel-reason",
    ];

    /// The backticked `W-…` ids of ADR-0055 §11.
    fn adr_ids() -> BTreeSet<String> {
        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/adr/0055-services-are-ha-blind.md");
        let adr = std::fs::read_to_string(path).expect("ADR-0055 reads");
        let start = adr.find("### 11. The proof").expect("ADR-0055 has §11");
        let end = adr[start..].find("\n### 12.").map_or(adr.len(), |e| start + e);
        adr[start..end]
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|s| s.starts_with("W-"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn every_section_11_bullet_and_every_row_share_one_id() {
        let rows: BTreeSet<String> =
            TABLE.iter().filter(|r| r.id.starts_with("W-")).map(|r| r.id.to_string()).collect();
        let adr = adr_ids();
        assert!(UNWITNESSED.iter().all(|id| rows.contains(*id)), "{UNWITNESSED:?}");
        let unstated: Vec<_> = rows.difference(&adr).collect();
        let rowless: Vec<_> = adr.difference(&rows).collect();
        assert!(unstated.is_empty(), "rows ADR-0055 §11 does not state: {unstated:?}");
        assert!(rowless.is_empty(), "ADR-0055 §11 ids no row carries: {rowless:?}");
    }

    #[test]
    fn ids_are_unique_and_no_row_opens_in_the_guaranteed_tier() {
        let mut seen = BTreeSet::new();
        for row in TABLE {
            assert!(seen.insert(row.id), "{} is twice in the table", row.id);
            assert!(row.id.starts_with("W-") || row.id.starts_with("T-"), "{}", row.id);
            assert_ne!(row.tier, Tier::Guaranteed, "{} opens in the Guaranteed tier", row.id);
        }
    }

    #[test]
    fn every_witness_is_a_shape_of_the_upstream_catalogue() {
        let sweep = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/it/sweep.rs");
        let sweep = std::fs::read_to_string(sweep).expect("the sweep's test file reads");
        for row in TABLE {
            let Some(w) = row.witness else { continue };
            let module = w.shape.replace('-', "_");
            assert!(
                sweep.contains(&format!("    {module} => ")),
                "{}'s witness shape {} is not in tests/it/sweep.rs",
                row.id,
                w.shape
            );
        }
    }

    #[test]
    fn every_row_but_a_transitional_or_unwitnessed_one_names_a_witness_its_incidents_strike() {
        for row in TABLE {
            let unwitnessed = row.id.starts_with("T-") || UNWITNESSED.contains(&row.id);
            match (unwitnessed, row.witness) {
                (true, None) => {}
                (true, Some(_)) => panic!("{} names a witness: off the unwitnessed list", row.id),
                (false, None) => panic!("{} names no witness cell", row.id),
                (false, Some(w)) => assert!(
                    row.opens.incidents().any(|i| i == w.incident),
                    "{}'s witness is cut by {:?}, which does not open it",
                    row.id,
                    w.incident
                ),
            }
        }
    }
}
