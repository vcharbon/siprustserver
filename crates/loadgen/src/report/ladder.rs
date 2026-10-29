//! One scenario's message ladder as the reporter folds it: every call's
//! arrows merged by key (bounded), each failure charged to the arrow it was
//! detected at, and the projection the index carries.

use std::collections::{BTreeMap, HashMap};

use e2e_model::{ClassCount, Ladder, LadderArrow};

use crate::class::{CallOutcome, ResultClass};
use crate::mux::{ArrowKey, ArrowStat, CallMessages, SUT_LANE};

/// The distinct arrows a scenario's ladder keeps; later ones fold into its
/// "other" row.
const MAX_KEYS: usize = 512;

/// The arrows a ladder shows; the rest fold into its "other" row.
pub(crate) const SHOWN_ARROWS: usize = 64;

#[derive(Default, Clone)]
struct ArrowAgg {
    stat: ArrowStat,
    errors: BTreeMap<String, u64>,
    /// Retransmissions of the request answered again after its call ended.
    late: u64,
}

impl ArrowAgg {
    fn add(&mut self, other: &ArrowAgg) {
        add_stat(&mut self.stat, &other.stat);
        self.late += other.late;
        for (class, n) in &other.errors {
            *self.errors.entry(class.clone()).or_default() += n;
        }
    }
}

fn add_stat(into: &mut ArrowStat, s: &ArrowStat) {
    into.count += s.count;
    into.duplicates += s.duplicates;
    into.resent += s.resent;
    into.offset_ms_sum += s.offset_ms_sum;
}

/// One scenario's ladder, over the calls folded so far.
#[derive(Default)]
pub(super) struct LadderAgg {
    calls: u64,
    /// The run configured the retransmit engine for the scenario.
    pub(super) engine: bool,
    /// An arrow was folded into "other" for want of room.
    truncated: bool,
    arrows: HashMap<ArrowKey, ArrowAgg>,
    other: ArrowAgg,
    after_call: BTreeMap<String, u64>,
    before_reply: BTreeMap<String, u64>,
    /// Copies received after the first, over the calls.
    pub(super) received: u64,
    /// Copies sent after the first, over the calls.
    pub(super) resent: u64,
}

impl LadderAgg {
    /// Calls folded in.
    pub(super) fn calls(&self) -> u64 {
        self.calls
    }

    /// A ladder for a scenario the run configured the retransmit engine
    /// for, or not.
    pub(super) fn new(engine: bool) -> Self {
        Self { engine, ..Self::default() }
    }

    /// Fold one call of `outcome`: its arrows, and its failure charged to the
    /// last arrow the failing agent's lane received before the outcome (the
    /// caller's when no agent failed); a verdict judged after the call
    /// (`rfc_audit_fail`, `check_fail`) has no arrow.
    pub(super) fn fold(&mut self, call: CallMessages, outcome: &CallOutcome) {
        self.calls += 1;
        if call.other.count > 0 {
            self.truncated = true;
            self.received += call.other.duplicates;
            self.other.add(&ArrowAgg { stat: call.other, ..ArrowAgg::default() });
        }
        for (key, stat) in &call.arrows {
            let (key, stat) = (key.clone(), *stat);
            self.received += stat.duplicates;
            self.resent += stat.resent;
            let agg = ArrowAgg { stat, ..ArrowAgg::default() };
            match self.slot(key) {
                Some(slot) => slot.add(&agg),
                None => {
                    self.truncated = true;
                    self.other.add(&agg);
                }
            }
        }
        let class = ResultClass::from(outcome);
        let label = class.label();
        match class {
            ResultClass::Ok | ResultClass::Rejected => {}
            ResultClass::RfcAuditFail | ResultClass::CheckFail => {
                *self.after_call.entry(label).or_default() += 1
            }
            _ => {
                // A barrier's own failure names no agent: the caller's lane.
                let who = match outcome {
                    CallOutcome::Step { at, .. } if at.who != at.barrier => at.who,
                    _ => call.caller.as_str(),
                };
                let errors = match call.charge_of(who) {
                    Some(Some(key)) => match self.arrows.get_mut(key) {
                        Some(slot) => &mut slot.errors,
                        None => &mut self.other.errors,
                    },
                    Some(None) => &mut self.other.errors,
                    None => &mut self.before_reply,
                };
                *errors.entry(label).or_default() += 1;
            }
        }
    }

    /// A retransmission of the request on `arrow`, answered again by the mux
    /// after its call ended (the call may not be folded yet); an arrow past
    /// the ladder's bound counts on "other". A repair whose request has no
    /// arrow (no tally, or its arrow past the call's bound) is on no ladder:
    /// the run's count is the mux's.
    pub(super) fn late(&mut self, arrow: &ArrowKey) {
        match self.slot(arrow.clone()) {
            Some(slot) => slot.late += 1,
            None => {
                self.truncated = true;
                self.other.late += 1;
            }
        }
    }

    fn slot(&mut self, key: ArrowKey) -> Option<&mut ArrowAgg> {
        if !self.arrows.contains_key(&key) && self.arrows.len() >= MAX_KEYS {
            return None;
        }
        Some(self.arrows.entry(key).or_default())
    }

    /// The ladder of `scenario`: the arrows by mean offset, the first
    /// [`SHOWN_ARROWS`] shown and the rest folded into "other"; `caller`
    /// names the caller's lane.
    pub(super) fn project(&self, scenario: &str, caller: &str) -> Ladder {
        let mut arrows: Vec<(&ArrowKey, &ArrowAgg)> = self.arrows.iter().collect();
        arrows.sort_by(|(ka, a), (kb, b)| mean(&a.stat).total_cmp(&mean(&b.stat)).then(ka.cmp(kb)));
        let mut other = self.other.clone();
        for (_, agg) in arrows.iter().skip(SHOWN_ARROWS) {
            other.add(agg);
        }
        let mut lanes = vec![caller.to_string(), SUT_LANE.to_string()];
        for (key, _) in &arrows {
            for lane in [&key.from, &key.to] {
                if !lanes.contains(lane) {
                    lanes.push(lane.clone());
                }
            }
        }
        let shown = arrows.iter().take(SHOWN_ARROWS).map(|(k, a)| arrow(k, a)).collect();
        let other =
            (other.stat.count > 0 || !other.errors.is_empty() || other.late > 0).then(|| {
                arrow(
                    &ArrowKey {
                        from: String::new(),
                        to: String::new(),
                        label: String::new(),
                        nth: 0,
                    },
                    &other,
                )
            });
        Ladder {
            scenario: scenario.to_string(),
            calls: self.calls,
            engine: self.engine,
            truncated: self.truncated,
            lanes,
            arrows: shown,
            other,
            after_call: counts(&self.after_call),
            before_reply: counts(&self.before_reply),
        }
    }
}

fn mean(s: &ArrowStat) -> f64 {
    if s.count == 0 {
        0.0
    } else {
        s.offset_ms_sum / s.count as f64
    }
}

fn arrow(key: &ArrowKey, agg: &ArrowAgg) -> LadderArrow {
    LadderArrow {
        from: key.from.clone(),
        to: key.to.clone(),
        label: key.label.clone(),
        nth: key.nth,
        count: agg.stat.count,
        retransmissions: agg.stat.duplicates,
        resent: agg.stat.resent,
        mean_offset_ms: mean(&agg.stat),
        late_answered: agg.late,
        errors: counts(&agg.errors),
    }
}

fn counts(m: &BTreeMap<String, u64>) -> Vec<ClassCount> {
    m.iter().map(|(class, count)| ClassCount { class: class.clone(), count: *count }).collect()
}

#[cfg(test)]
#[path = "ladder_tests.rs"]
mod tests;
