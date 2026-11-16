//! The join of a run's `call-outcome.v1` records to what each call was drawn
//! as, by the seed its Call-ID stamp names.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::stamp::{seed_of_call_id, Drawn};

/// A run's records, each joined to its draw.
#[derive(Debug, Default)]
pub struct Joined {
    /// Every record as `{record, seed, draw}`; `seed` and `draw` are null for
    /// a record whose Call-ID names no drawn seed.
    pub lines: Vec<Value>,
    /// The joined lines of the failed records: one with a failure, or whose
    /// ending is `failed` or `timeout`.
    pub failed: Vec<Value>,
    /// Records by `<shape>/<ending>/<result or ending>`.
    pub classes: BTreeMap<String, u64>,
}

/// Whether `record` ran its call as its shape says.
pub fn record_passed(record: &Value) -> bool {
    record["failures"].as_array().is_none_or(|f| f.is_empty())
        && record["ending"] != "failed"
        && record["ending"] != "timeout"
}

/// Join `records` to `draws` (by token, as [`crate::drawn`] holds them).
pub fn join(records: &[Value], draws: &BTreeMap<String, Drawn>) -> Joined {
    let by_seed: BTreeMap<u64, &Drawn> = draws.values().map(|d| (d.seed, d)).collect();
    let mut joined = Joined::default();
    for r in records {
        let draw = r["call_id"].as_str().and_then(seed_of_call_id).and_then(|s| by_seed.get(&s));
        let text = |k: &str| r[k].as_str().unwrap_or("?").to_string();
        let class = r["result"].as_str().map_or_else(|| text("ending"), str::to_string);
        *joined
            .classes
            .entry(format!("{}/{}/{class}", text("shape"), text("ending")))
            .or_default() += 1;
        let line = json!({
            "record": r,
            "seed": draw.map(|d| format!("{:#x}", d.seed)),
            "draw": draw.map(|d| d.desc.clone()),
        });
        if !record_passed(r) {
            joined.failed.push(line.clone());
        }
        joined.lines.push(line);
    }
    joined
}

impl Joined {
    /// Write `joined.jsonl` and `failed.jsonl` under `dir`, one line each.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        let jsonl = |lines: &[Value]| lines.iter().map(|l| format!("{l}\n")).collect::<String>();
        std::fs::write(dir.join("joined.jsonl"), jsonl(&self.lines))?;
        std::fs::write(dir.join("failed.jsonl"), jsonl(&self.failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_joins_the_draw_its_call_id_names_and_a_failure_is_listed() {
        let draws = BTreeMap::from([(
            "t1".to_string(),
            Drawn { seed: 0xab, desc: "held fk2 | caller BYE".into() },
        )]);
        let records = vec![
            json!({ "call_id": "fz-00000000000000ab-0-1@fuzz.invalid", "shape": "s",
                    "ending": "answered", "failures": [] }),
            json!({ "call_id": "other@host", "shape": "s", "ending": "timeout", "failures": [] }),
        ];
        let j = join(&records, &draws);
        assert_eq!(j.lines[0]["draw"], "held fk2 | caller BYE");
        assert_eq!(j.lines[0]["seed"], "0xab");
        assert_eq!(j.lines[1]["draw"], Value::Null);
        assert_eq!(j.failed, vec![j.lines[1].clone()]);
        assert_eq!(
            j.classes,
            BTreeMap::from([("s/answered/answered".into(), 1), ("s/timeout/timeout".into(), 1)])
        );
    }
}
