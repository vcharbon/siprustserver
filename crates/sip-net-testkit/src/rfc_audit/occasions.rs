//! The per-rule population of one audit pass: every occasion a rule met at the
//! vantage the adapter surfaces it at, violated or not. The adapter notes each
//! one while [`tallied`] runs a rule; outside it, noting is a no-op.

use std::cell::RefCell;
use std::collections::HashMap;

use layer_harness::LaneKey;
use rfc_rules::{Finding, Population};

/// Per `(rule token, vantage bind)`, the population one rule pass noted.
pub(crate) type Tally = HashMap<(&'static str, LaneKey), Population>;

thread_local! {
    static TALLY: RefCell<Option<Tally>> = const { RefCell::new(None) };
}

/// Run `pass` and return what it returned with the occasions it noted.
pub(crate) fn tallied<R>(pass: impl FnOnce() -> R) -> (R, Tally) {
    TALLY.with(|t| *t.borrow_mut() = Some(Tally::new()));
    let out = pass();
    let tally = TALLY.with(|t| t.borrow_mut().take()).unwrap_or_default();
    (out, tally)
}

/// Note one occasion `f` surfaced at `bind`'s vantage.
pub(crate) fn note(bind: &LaneKey, f: &Finding) {
    TALLY.with(|t| {
        if let Some(tally) = t.borrow_mut().as_mut() {
            tally.entry((f.rule.token(), bind.clone())).or_default().count(f);
        }
    });
}
