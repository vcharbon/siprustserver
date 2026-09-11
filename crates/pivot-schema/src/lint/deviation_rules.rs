//! Deviation payload consistency: a `kind` and the fields beside it must
//! describe one violation.
//!
//! `kind` is an open token, so nothing here constrains a vocabulary this crate
//! does not own. What it does constrain is the four kinds the format DOES
//! define: a `cseq-override` with no value overrides nothing, and a
//! `suppress-auto` pointing at a scripted step withholds nothing — the
//! interpreter never composed that message, so there is nothing to withhold.
//! Each reads as deliberate non-compliance and reproduces none. A
//! `verbatim-emission` on an auto step is NOT among them: such a step stores
//! what any step stores (§6.3), so there is a block to preserve.
//!
//! These mirror the interpreter's plan refusals, so a generator learns at lint
//! time rather than at run time.

use crate::lint::{at, Index, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for deviation in &index.pivot.deviations {
        let path = at("deviations", &deviation.id);
        match deviation.kind.as_str() {
            "cseq-override" if deviation.value.is_none() => report.error(
                "deviation/cseq-override-no-value",
                &path,
                "`cseq-override` states no value",
                "state `value`: a number, or `{ from, delta }` against a CSeq the run saw",
            ),
            "suppress-auto" => match deviation.step.as_deref() {
                None => report.error(
                    "deviation/suppress-auto-no-step",
                    &path,
                    "`suppress-auto` names no step",
                    "point `step` at the auto step being withheld",
                ),
                Some(id) => {
                    if index.steps.get(id).is_some_and(|(_, step)| !step.auto) {
                        report.error(
                            "deviation/suppress-auto-not-auto",
                            &path,
                            format!("`suppress-auto` names {id:?}, which is not an automatic"),
                            "only a stack-composed step can be withheld; a scripted one is simply not written",
                        );
                    }
                }
            },
            _ => {}
        }
    }
}
