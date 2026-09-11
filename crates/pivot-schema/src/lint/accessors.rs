//! Accessor context: a `${…}` must name something that exists, and something
//! the run will already KNOW by the time the accessor is resolved.
//!
//! Leg accessors read runner dialog state, so they only need the leg to be
//! declared. Early-dialog accessors read ONE fork's state, so they need some
//! step to declare that `early` id — and to declare it on ONE leg, since a leg
//! owns its own fork tag space and the same id on two legs is two dialogs.
//! Number accessors read the lane's binding of a registered identity,
//! so they need the identity to be registered AND to declare the dial form
//! asked for — a lane can only bind a form the numbering plan resolved. Step
//! accessors read a message, so they need that message to have
//! happened — and to have happened on EVERY run that reaches the accessor,
//! which is why a reference into an `alt` branch is refused from anywhere but
//! that branch. `${step:<alt-id>.branch}` obeys the same rule: which branch ran
//! is not known until the alt has completed.

use crate::accessor::{Accessor, StepField};
use crate::deviation::CseqValue;
use crate::lint::strings::{deviation_strings, node_strings, postcondition_strings, step_strings};
use crate::lint::{at, reach, Index, Place, Reach, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for (place, node) in index.pivot.flow.iter().enumerate().map(|(n, e)| (Place::node(n), e)) {
        let path = at("flow", node.id());
        for found in node_strings(node) {
            scan(index, report, &format!("{path}.{}", found.field), place, found.text);
        }
    }
    for (place, step) in index.all_steps() {
        let path = at("flow", &step.id);
        for found in step_strings(step) {
            scan(index, report, &format!("{path}.{}", found.field), place, found.text);
        }
    }

    if let Some(postconditions) = &index.pivot.postconditions {
        for found in postcondition_strings(postconditions) {
            let path = format!("postconditions.{}", found.field);
            scan(index, report, &path, Place::AFTER_EVERYTHING, found.text);
        }
    }

    for deviation in &index.pivot.deviations {
        let path = at("deviations", &deviation.id);
        // The deviation applies at its own step, so a value it names must be
        // known by then — not merely by the end of the run.
        let place = deviation
            .step
            .as_deref()
            .and_then(|id| index.place_of(id))
            .unwrap_or(Place::AFTER_EVERYTHING);
        for found in deviation_strings(deviation) {
            scan(index, report, &format!("{path}.{}", found.field), place, found.text);
        }
        if let Some(CseqValue::Relative(computed)) = &deviation.value {
            resolve(index, report, &format!("{path}.value.from"), place, &computed.from);
        }
    }
}

fn scan(index: &Index<'_>, report: &mut Report, path: &str, place: Place, text: &str) {
    for found in Accessor::scan(text) {
        match found {
            Ok(accessor) => resolve(index, report, path, place, &accessor),
            Err(error) => report.error(
                "accessor/malformed",
                path,
                error,
                "use `${leg:<id>.<field>}`, `${early:<id>.<field>}`, `${step:<id>.<field>}` or `${num:<identity>:<form>}`",
            ),
        }
    }
}

fn resolve(index: &Index<'_>, report: &mut Report, path: &str, place: Place, accessor: &Accessor) {
    match accessor {
        Accessor::Leg { leg, .. } => {
            if !index.legs.contains(leg.as_str()) {
                report.error(
                    "accessor/leg-unknown",
                    path,
                    format!("{accessor} names leg {leg:?}, which is not declared"),
                    "name a leg from `legs`",
                );
            }
        }
        Accessor::Early { early, .. } => match index.early.get(early.as_str()) {
            None => report.error(
                "accessor/early-unknown",
                path,
                format!("{accessor} names early dialog {early:?}, which no step declares"),
                "name an `early` id some step on the forking leg carries",
            ),
            Some(legs) if legs.len() > 1 => report.error(
                "accessor/early-ambiguous",
                path,
                format!(
                    "{accessor} names early dialog {early:?}, which legs {} each declare; \
                     a leg owns its own fork tags, so that is two dialogs",
                    legs.iter().copied().collect::<Vec<_>>().join(", ")
                ),
                "give each leg's fork its own `early` id",
            ),
            Some(_) => {}
        },
        Accessor::Number { name, form } => match index.identities.get(name.as_str()) {
            None => report.error(
                "accessor/identity-unknown",
                path,
                format!("{accessor} names identity {name:?}, which is not in the registry"),
                "name an entry from `identities`",
            ),
            Some(identity) if !identity.has_form(form) => report.error(
                "accessor/num-form-unknown",
                path,
                format!("identity {name:?} states no dial form {form:?}"),
                "ask for a form the identity declares; a lane can only bind one the plan resolved",
            ),
            Some(_) => {}
        },
        Accessor::Step { step, field } => {
            if *field == StepField::Branch && !index.alts.contains(step.as_str()) {
                report.error(
                    "accessor/branch-on-non-alt",
                    path,
                    format!("{accessor} asks which branch ran, but {step:?} is no `alt`"),
                    "only an `alt` node reports a branch; name one",
                );
                return;
            }
            match index.place_of(step) {
                None => report.error(
                    "accessor/step-unknown",
                    path,
                    format!("{accessor} names {step:?}, which is no step or block"),
                    "name a step or block declared in `flow`",
                ),
                Some(target) => match reach(target, place) {
                    Reach::Ok => {}
                    Reach::Forward => report.error(
                        "accessor/forward-step",
                        path,
                        format!("{accessor} reads something that has not run yet"),
                        "read a value produced by something that precedes this",
                    ),
                    Reach::CrossBranch => report.error(
                        "accessor/cross-branch",
                        path,
                        format!("{accessor} reads a step inside an `alt` branch this is not part of"),
                        "a branch step exists only on the run that chose it; read the alt's own id, or a step of the same branch",
                    ),
                },
            }
        }
    }
}
