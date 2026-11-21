//! The join of a run's `call-outcome.v1` records to what each call was drawn
//! as, by the Call-ID the draw stamped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::stamp::{Drawn, Requester};

/// A run's records, each joined to its draw.
#[derive(Debug, Default)]
pub struct Joined {
    /// Every record as `{record, seed, draw}`; `seed` and `draw` are null for
    /// a record whose Call-ID no draw carries.
    pub lines: Vec<Value>,
    /// The joined lines of the failed records ([`record_passed`]).
    pub failed: Vec<Value>,
    /// The records no draw carries the Call-ID of.
    pub unjoined: Vec<Value>,
    /// The draws no record carries the Call-ID of: calls drawn, never recorded.
    pub undrawn: Vec<Drawn>,
    /// Records by `<shape>/<ending>/<producer_verdict>`.
    pub classes: BTreeMap<String, u64>,
}

/// Whether `record` ran its call to an ending the call accepts
/// (`producer_verdict` `ok:<branch>`) with no failure in it but the refused
/// in-dialog requests its draw asked for ([`Drawn::refusals`]).
pub fn record_passed(record: &Value, draw: Option<&Drawn>) -> bool {
    let side = |f: &Value, r: Requester| match r {
        Requester::Caller => f["leg"].is_null(),
        Requester::Callee => !f["leg"].is_null(),
        Requester::Either => true,
    };
    let asked = |f: &Value| {
        f["kind"] == "dialog_request"
            && draw.is_some_and(|d| {
                d.refusals
                    .iter()
                    .any(|(r, m, c)| side(f, *r) && f["method"] == *m && f["code"] == *c)
            })
    };
    record["failures"].as_array().is_none_or(|f| f.iter().all(asked))
        && record["producer_verdict"].as_str().is_some_and(|v| v.starts_with("ok:"))
}

/// Join `records` to `draws` (by token, as [`crate::drawn`] holds them), each
/// record to the draw whose Call-ID it carries.
pub fn join(records: &[Value], draws: &BTreeMap<String, Drawn>) -> Joined {
    let by_call_id: BTreeMap<&str, &Drawn> =
        draws.values().map(|d| (d.call_id.as_str(), d)).collect();
    let mut recorded = BTreeSet::new();
    let mut joined = Joined::default();
    for r in records {
        let draw = r["call_id"].as_str().and_then(|id| by_call_id.get(id).copied());
        let text = |k: &str| r[k].as_str().unwrap_or("?").to_string();
        let class = format!("{}/{}/{}", text("shape"), text("ending"), text("producer_verdict"));
        *joined.classes.entry(class).or_default() += 1;
        let line = json!({
            "record": r,
            "seed": draw.map(|d| format!("{:#x}", d.seed)),
            "draw": draw.map(|d| d.desc.clone()),
        });
        match draw {
            Some(d) => {
                recorded.insert(d.call_id.as_str());
            }
            None => joined.unjoined.push(r.clone()),
        }
        if !record_passed(r, draw) {
            joined.failed.push(line.clone());
        }
        joined.lines.push(line);
    }
    joined.undrawn =
        draws.values().filter(|d| !recorded.contains(d.call_id.as_str())).cloned().collect();
    joined
}

impl Joined {
    /// Every record joined to a draw and every draw recorded.
    pub fn complete(&self) -> bool {
        self.unjoined.is_empty() && self.undrawn.is_empty()
    }

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

    fn draw(seed: u64, call_id: &str) -> Drawn {
        Drawn {
            seed,
            call_id: call_id.into(),
            desc: format!("draw {seed:#x}"),
            refusals: vec![(Requester::Caller, "INVITE", 488)],
        }
    }

    fn record(call_id: &str, ending: &str, verdict: &str) -> Value {
        json!({ "call_id": call_id, "shape": "s", "ending": ending, "failures": [],
                "producer_verdict": verdict })
    }

    /// Two calls replaying one seed join their own draws; a record no draw
    /// carries, and a draw no record carries, are both named.
    #[test]
    fn each_record_joins_the_draw_of_its_call_id() {
        let draws = BTreeMap::from([
            ("t1".to_string(), draw(0xab, "fz-00000000000000ab-0-1@fuzz.invalid")),
            ("t2".to_string(), draw(0xab, "fz-00000000000000ab-1-1@fuzz.invalid")),
            ("t3".to_string(), draw(0xcd, "fz-00000000000000cd-2-1@fuzz.invalid")),
        ]);
        let mut first = record("fz-00000000000000ab-1-1@fuzz.invalid", "answered", "ok:answered");
        first["shape"] = json!("first");
        let records = vec![
            first,
            record("fz-00000000000000ab-0-1@fuzz.invalid", "answered", "ok:answered"),
            record("other@host", "answered", "ok:answered"),
        ];
        let j = join(&records, &draws);
        assert_eq!(j.lines[0]["record"]["shape"], "first");
        assert_eq!(j.lines[0]["seed"], "0xab");
        assert_eq!(j.lines[0]["draw"], "draw 0xab");
        assert_eq!(j.lines[2]["draw"], Value::Null);
        assert_eq!(j.unjoined, vec![records[2].clone()]);
        assert_eq!(j.undrawn, vec![draws["t3"].clone()]);
        assert!(!j.complete());
    }

    /// A record passes only on an accepted ending with no failure: a final
    /// the call does not accept fails it, whatever its ending, and so does a
    /// failure on an accepted one.
    #[test]
    fn a_record_passes_only_on_an_accepted_ending_with_no_failure() {
        let ids = ["fz-0000000000000001-0-1@x", "fz-0000000000000002-1-1@x"];
        let draws = BTreeMap::from([
            ("t1".to_string(), draw(1, ids[0])),
            ("t2".to_string(), draw(2, ids[1])),
        ]);
        let mut audited = record(ids[1], "answered", "ok:answered");
        audited["failures"] = json!([{ "at_ms": 9, "kind": "rfc_audit" }]);
        let records = vec![record(ids[0], "rejected", "status_500"), audited];
        let j = join(&records, &draws);
        assert!(j.complete());
        assert_eq!(j.failed, j.lines);
        assert_eq!(
            j.classes,
            BTreeMap::from([
                ("s/answered/ok:answered".into(), 1),
                ("s/rejected/status_500".into(), 1)
            ])
        );
        let passing = record(ids[0], "abandoned", "ok:abandoned");
        assert!(join(&[passing], &draws).failed.is_empty());
    }

    /// A refused in-dialog request the draw asked for passes; another code,
    /// or a refusal of another method, still fails the record.
    #[test]
    fn a_drawn_refusal_passes_and_no_other() {
        let id = "fz-0000000000000003-0-1@x";
        let d = draw(3, id);
        let refused = |method: &str, code: u16| {
            let mut r = record(id, "answered", "ok:answered");
            r["failures"] =
                json!([{ "at_ms": 9, "kind": "dialog_request", "method": method, "code": code }]);
            r
        };
        assert!(record_passed(&refused("INVITE", 488), Some(&d)));
        let mut on_a_leg = refused("INVITE", 488);
        on_a_leg["failures"][0]["leg"] = json!("bob");
        assert!(
            !record_passed(&on_a_leg, Some(&d)),
            "the drawn refusal meets the caller's request"
        );
        assert!(!record_passed(&refused("INVITE", 500), Some(&d)));
        assert!(!record_passed(&refused("UPDATE", 488), Some(&d)));
        assert!(!record_passed(&refused("INVITE", 488), None));
    }
}
