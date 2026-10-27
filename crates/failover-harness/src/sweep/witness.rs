//! The dead-window check (ADR-0055 §11): every row of the window table names
//! an upstream witness cell, and the test holding that cell checks the row
//! fires there. A row no
//! cell fires any more is a window the code no longer needs, or one a change
//! stopped exercising.

use b2bua::repl::SyncLevel;

use super::{Incident, Ran};
use crate::turn_points::TurnAt;
use crate::windows::TABLE;

/// One line per row whose witness is a cell of `shape` cut by `incident` at
/// `level` and did not fire there: the cell did not run, or ran without it.
pub(super) fn dead_windows(
    shape: &str,
    incident: Incident,
    level: SyncLevel,
    ran: &[Ran],
) -> Vec<String> {
    let witnessed = TABLE.iter().filter_map(|row| {
        let w = row.witness?;
        (w.shape == shape && w.incident == incident && w.level == level)
            .then_some((row, TurnAt { turn: w.turn, point: w.point }))
    });
    witnessed
        .filter_map(|(row, at)| match ran.iter().find(|r| r.at == at) {
            Some(r) if r.measured.fired.contains(&row.id) => None,
            Some(r) => {
                Some(format!("a dead window: {} does not fire in its witness {}", row.id, r.name))
            }
            None => Some(format!("a dead window: {}'s witness {} did not run", row.id, at.label())),
        })
        .collect()
}
