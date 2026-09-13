//! The retransmission schedules the pivot's ladders ride, as DATA
//! (`pivot-schema schedules`): for every [`sip_retransmit::Class`], the wait
//! before each rung the RFC puts on the wire before the class gives up, and
//! the give-up itself, in milliseconds.
//!
//! Exported so a generator that projects a ladder onto a timeline — the cut's
//! drawn-ACK count asks how many copies of a final land before the ACK — reads
//! the one schedule the SUT and the interpreter ride instead of walking T1/T2
//! a second time (ADR-0032 X1). The table is finite because every class gives
//! up, and the walk is [`sip_retransmit::Ladder`]'s own, never restated here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sip_retransmit::{Class, Ladder, Schedule};

/// Every class's schedule, as one exportable document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTable {
    /// One row per class, in [`Class::ALL`] order.
    pub classes: Vec<ClassSchedule>,
}

/// One class's ladder, walked from the original send to its give-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassSchedule {
    /// The class's stable label ([`Class::as_str`]), the same one a counter
    /// names it by.
    pub class: String,
    /// The wait before each rung, rung 1 first: every rung that lands before
    /// the give-up, and no other.
    pub rung_intervals_ms: Vec<u64>,
    /// The bound, measured from the original send: no rung lands at or past it.
    pub give_up_ms: u64,
}

/// The table, one row per class.
pub fn schedule_table() -> ScheduleTable {
    ScheduleTable { classes: Class::ALL.iter().map(|class| row(*class)).collect() }
}

fn row(class: Class) -> ClassSchedule {
    let schedule = Schedule::rfc(class);
    let give_up_ms = millis(schedule.give_up_after());
    let mut rung_intervals_ms = Vec::new();
    if let Some((mut ladder, first)) = Ladder::armed(schedule) {
        rung_intervals_ms.push(millis(first));
        while let Some(wait) = ladder.advance() {
            rung_intervals_ms.push(millis(wait));
        }
    }
    ClassSchedule { class: class.as_str().to_string(), rung_intervals_ms, give_up_ms }
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rung lands before the give-up, and the next would not: the table
    /// is exactly the ladder the class walks, no rung short and none past.
    #[test]
    fn every_row_is_walked_to_its_give_up_and_no_further() {
        let table = schedule_table();
        assert_eq!(table.classes.len(), Class::ALL.len());
        for (row, class) in table.classes.iter().zip(Class::ALL) {
            assert_eq!(row.class, class.as_str());
            let elapsed: u64 = row.rung_intervals_ms.iter().sum();
            assert!(
                elapsed < row.give_up_ms,
                "{}: the last rung lands inside the bound",
                row.class
            );
            let schedule = Schedule::rfc(class);
            let next = u32::try_from(row.rung_intervals_ms.len() + 1).expect("a short ladder");
            let past =
                elapsed + millis(schedule.interval(next).expect("an RFC class always paces"));
            assert!(
                past >= row.give_up_ms,
                "{}: one more rung would land at or past the bound",
                row.class
            );
        }
    }

    #[test]
    fn the_capped_classes_flatten_at_t2_and_the_uncapped_keep_doubling() {
        let table = schedule_table();
        let by = |name: &str| {
            &table.classes.iter().find(|r| r.class == name).expect(name).rung_intervals_ms
        };
        assert_eq!(by("invite-client"), &[500, 1000, 2000, 4000, 8000, 16000]);
        assert_eq!(by("reliable-provisional"), &[500, 1000, 2000, 4000, 8000, 16000]);
        assert_eq!(by("final-2xx"), &[500, 1000, 2000, 4000, 4000, 4000, 4000, 4000, 4000, 4000]);
        assert_eq!(by("non-invite-proceeding"), &[4000; 7]);
    }
}
