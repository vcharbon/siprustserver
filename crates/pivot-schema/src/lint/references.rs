//! Referential integrity and ordering: every id a document names resolves
//! inside the same document, no id is claimed twice, and every reference points
//! BACKWARDS.
//!
//! Backwards matters more than it looks. A delay anchored on a later step is
//! underivable, and an `after` naming a later node is a deadlock the runner
//! would only discover by timing out — one of the few classes of mistake a
//! document can hold that costs a whole test run to find.

use std::collections::BTreeSet;

use crate::call::Position;
use crate::flow::{Anchor, FlowNode, Op, Step};
use crate::lint::{at, reach, Index, Place, Reach, Report};
use crate::msg::Ref;

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    unique_ids(index, report);
    placement(index, report);
    flow_refs(index, report);
    positions(index, report);
    document_refs(index, report);
}

fn unique_ids(index: &Index<'_>, report: &mut Report) {
    let pivot = index.pivot;
    let mut groups: Vec<(&str, Vec<&str>)> = vec![
        ("endpoints", pivot.endpoints.iter().map(|e| e.id.as_str()).collect()),
        ("actors", pivot.actors.iter().map(|a| a.id.as_str()).collect()),
        ("legs", pivot.legs.iter().map(|l| l.id.as_str()).collect()),
        ("identities", pivot.identities.iter().map(|i| i.name.as_str()).collect()),
        ("calls", pivot.calls.iter().map(|c| c.id.as_str()).collect()),
        ("deviations", pivot.deviations.iter().map(|d| d.id.as_str()).collect()),
    ];
    // A block id and a step id share one namespace: `after` and the accessor
    // grammar name either, so a collision makes a reference ambiguous.
    let mut flow_ids: Vec<&str> = Vec::new();
    for node in &pivot.flow {
        // A message node IS its step, so its id is counted once, through the
        // steps it reports.
        if !matches!(node, FlowNode::Message(_)) {
            flow_ids.push(node.id());
        }
        flow_ids.extend(node.steps().iter().map(|s| s.id.as_str()));
    }
    groups.push(("flow", flow_ids));

    for (kind, ids) in groups {
        let mut seen = BTreeSet::new();
        for id in ids {
            if id.is_empty() {
                report.error(
                    "id/empty",
                    kind,
                    "an entry states an empty id",
                    "give every entry an id other references can name",
                );
            } else if !seen.insert(id) {
                report.error(
                    "id/duplicate",
                    at(kind, id),
                    format!("id {id:?} is claimed twice"),
                    "ids are the document's only reference mechanism; make each one unique",
                );
            }
            if id.contains('.') {
                report.error(
                    "id/dot",
                    at(kind, id),
                    format!("id {id:?} contains a `.`"),
                    "the accessor grammar splits an id from its field at the first `.`; rename it",
                );
            }
        }
    }

    identities(index, report);
}

/// `${num:<name>:<form>}` splits on `:` and closes on `}`, so both halves must
/// be spellable inside it — and a position-derived name must say WHICH chain it
/// positions, exactly as a tier-2 position token does.
fn identities(index: &Index<'_>, report: &mut Report) {
    let multi_call = index.pivot.calls.len() > 1;
    for identity in &index.pivot.identities {
        let path = at("identities", &identity.name);
        if reserved(&identity.name) {
            report.error(
                "id/colon",
                &path,
                format!("identity name {:?} carries `:`, `{{` or `}}`", identity.name),
                "the `${num:<name>:<form>}` grammar reserves them; rename the identity",
            );
        }
        for form in &identity.forms {
            if reserved(form) || form.is_empty() {
                report.error(
                    "id/form-colon",
                    &path,
                    format!("dial form {form:?} carries `:`, `{{` or `}}`, or is empty"),
                    "a form is the second half of `${num:<name>:<form>}`; rename it",
                );
            }
        }
        if multi_call && is_bare_position(&identity.name) {
            report.error(
                "id/identity-unqualified",
                &path,
                format!(
                    "identity name {:?} is the bare position form in a document with several calls",
                    identity.name
                ),
                "qualify it with the call id: `<call-id>-caller`, `<call-id>-called-<b>-<p>`",
            );
        }
    }
}

/// Whether a token carries a character the number-accessor grammar owns.
fn reserved(token: &str) -> bool {
    token.contains([':', '{', '}'])
}

/// Whether a name is the UNQUALIFIED position form the generator synthesizes —
/// `caller` or `called-<branch>-<position>`. An authored name that positions
/// nothing (`transferee`) is not one, and needs no call id.
fn is_bare_position(name: &str) -> bool {
    if name == "caller" {
        return true;
    }
    match name.strip_prefix("called-").and_then(|rest| rest.split_once('-')) {
        Some((branch, position)) => {
            branch.parse::<u32>().is_ok() && position.parse::<u32>().is_ok()
        }
        None => false,
    }
}

fn placement(index: &Index<'_>, report: &mut Report) {
    for actor in &index.pivot.actors {
        if !index.endpoints.contains(actor.endpoint.as_str()) {
            report.error(
                "ref/actor-endpoint-unknown",
                at("actors", &actor.id),
                format!("endpoint {:?} is not declared", actor.endpoint),
                "name an endpoint from `endpoints`",
            );
        }
        if let Some(identity) = &actor.identity {
            if !index.identities.contains_key(identity.as_str()) {
                report.error(
                    "ref/actor-identity-unknown",
                    at("actors", &actor.id),
                    format!("`identity` names {identity:?}, which is not in the registry"),
                    "name an entry from `identities`",
                );
            }
        }
    }
    for leg in &index.pivot.legs {
        if !index.actors.contains(leg.actor.as_str()) {
            report.error(
                "ref/leg-actor-unknown",
                at("legs", &leg.id),
                format!("actor {:?} is not declared", leg.actor),
                "name an actor from `actors`",
            );
        }
    }
}

fn flow_refs(index: &Index<'_>, report: &mut Report) {
    for (position, node) in index.pivot.flow.iter().enumerate() {
        let path = at("flow", node.id());
        for target in node.after() {
            ordering(index, report, &path, "`after`", target, Place::node(position), AFTER);
        }
        if let FlowNode::Inject(inject) = node {
            if let Some(delay) = &inject.delay {
                anchor(index, report, &path, node.id(), &delay.from);
            }
        }
    }

    for (place, step) in index.all_steps() {
        let path = at("flow", &step.id);
        if !index.legs.contains(step.leg.as_str()) {
            report.error(
                "ref/step-leg-unknown",
                &path,
                format!("leg {:?} is not declared", step.leg),
                "name a leg from `legs`",
            );
        }
        for target in &step.after {
            ordering(index, report, &path, "`after`", target, place, AFTER);
        }
        anchor(index, report, &path, &step.id, &step.delay.from);
        if let Some(other) = &step.overlap {
            overlap(index, report, &path, step, other);
        }
    }
}

/// A declared race holds only where a run can arm it: two steps on ONE leg,
/// NEIGHBOURS in that leg's order, the field on the LATER of the pair, and the
/// pair in one of the two shapes a race is made of (§6.7a). Across legs there
/// is no order to revoke, over a gap the skipped steps' order is one the
/// document states, and a stamp read before its target has run does not resolve.
fn overlap(index: &Index<'_>, report: &mut Report, path: &str, step: &Step, other: &str) {
    let Some((_, target)) = index.steps.get(other) else {
        report.error(
            "ref/overlap-unknown",
            path,
            format!("`overlap` names {other:?}, which is no step"),
            "name the step this one races with",
        );
        return;
    };
    if target.leg != step.leg {
        report.error(
            "ref/overlap-cross-leg",
            path,
            format!(
                "`overlap` names {other:?} on leg {:?}, where this step rides {:?}",
                target.leg, step.leg
            ),
            "race two steps on one leg; legs already run independently",
        );
        return;
    }
    let on_leg: Vec<&str> = index
        .pivot
        .flow
        .iter()
        .flat_map(|node| node.steps())
        .filter(|s| s.leg == step.leg)
        .map(|s| s.id.as_str())
        .collect();
    let position = |id: &str| on_leg.iter().position(|s| *s == id);
    let places = (position(&step.id), position(other));
    let adjacent = matches!(places, (Some(a), Some(b)) if a.abs_diff(b) == 1);
    if !adjacent {
        report.error(
            "ref/overlap-not-adjacent",
            path,
            format!("`overlap` names {other:?}, which is not this step's neighbour on the leg"),
            "race a step with the one beside it",
        );
        return;
    }
    if matches!(places, (Some(a), Some(b)) if a < b) {
        report.error(
            "ref/overlap-forward",
            path,
            format!("`overlap` names {other:?}, which runs after this step on leg {:?}", step.leg),
            "the LATER step of the pair carries the field; stamp the race there",
        );
    }
    if !races(step, target) {
        report.error(
            "ref/overlap-no-race",
            path,
            format!(
                "`overlap` names {other:?}: the two are measured from different anchors ({} and {}) and are not both arrivals",
                step.delay.from, target.delay.from
            ),
            "a race is a `send` and an arrival on ONE anchor, or two arrivals; drop the field",
        );
    }
}

/// The two shapes a race is made of (§6.7a): a `send` and an arrival measured
/// from ONE anchor, where the captured order is a dwell the document holds
/// against a latency it does not; or two ARRIVALS, whose separate anchors are
/// what makes the transactions independent.
fn races(step: &Step, other: &Step) -> bool {
    step.delay.from == other.delay.from || (step.op == Op::Expect && other.op == Op::Expect)
}

fn anchor(index: &Index<'_>, report: &mut Report, path: &str, id: &str, from: &Anchor) {
    let Some(target) = from.step() else { return };
    if target == id {
        report.error(
            "order/anchor-self",
            path,
            "the delay anchors on the step itself",
            "anchor on `trigger` or on an earlier step",
        );
        return;
    }
    let Some(place) = index.place_of(id) else { return };
    ordering(index, report, path, "delay anchor", target, place, ANCHOR);
}

/// Rule ids for one kind of reference: unknown target, forward target, target
/// inside an `alt` branch the reference is not part of.
struct Rules {
    unknown: &'static str,
    forward: &'static str,
    what: &'static str,
}

const AFTER: Rules =
    Rules { unknown: "ref/after-unknown", forward: "order/after-forward", what: "orders after" };
const ANCHOR: Rules = Rules {
    unknown: "ref/anchor-unknown",
    forward: "order/anchor-forward",
    what: "is measured from",
};

/// One reference must name something declared, that already ran, on every run
/// that reaches it.
fn ordering(
    index: &Index<'_>,
    report: &mut Report,
    path: &str,
    label: &str,
    target: &str,
    from: Place,
    rules: Rules,
) {
    let Some(place) = index.place_of(target) else {
        report.error(
            rules.unknown,
            path,
            format!("{label} names {target:?}, which is no step or block"),
            "name a step or block id declared in `flow`",
        );
        return;
    };
    match reach(place, from) {
        Reach::Ok => {}
        Reach::Forward => report.error(
            rules.forward,
            path,
            format!("{label} names {target:?}, which does not run earlier"),
            format!("a step {} something that already happened", rules.what),
        ),
        Reach::CrossBranch => report.error(
            "order/cross-branch",
            path,
            format!("{label} names {target:?}, a step inside an `alt` branch this is not part of"),
            "a branch step exists only on the run that chose it; name the alt's own id, or a step of the same branch",
        ),
    }
}

/// Tier-2 positional refs resolve against the call chains. A bare `called[b][s]`
/// names one chain, so a document with several calls must qualify it.
fn positions(index: &Index<'_>, report: &mut Report) {
    let multi_call = index.pivot.calls.len() > 1;
    for (_, step) in index.all_steps() {
        let path = at("flow", &step.id);
        for reference in [&step.msg.ruri, &step.msg.from, &step.msg.to].into_iter().flatten() {
            let Ref::Positional(positional) = reference else { continue };
            let parsed = match Position::parse(&positional.pos) {
                Ok(parsed) => parsed,
                Err(error) => {
                    report.error(
                        "ref/pos-malformed",
                        &path,
                        error,
                        "use `caller` or `called[branch][position]`, call-qualified where needed",
                    );
                    continue;
                }
            };
            if multi_call && parsed.call.is_none() {
                report.error(
                    "ref/pos-unqualified",
                    &path,
                    format!(
                        "position {:?} is bare in a document with several calls",
                        positional.pos
                    ),
                    "qualify it with the call id: `<call-id>.called[b][s]`",
                );
                continue;
            }
            let call = match &parsed.call {
                Some(id) => index.pivot.calls.iter().find(|c| &c.id == id),
                None => index.pivot.calls.first(),
            };
            let Some(call) = call else {
                report.error(
                    "ref/pos-unknown",
                    &path,
                    format!("position {:?} names no declared call", positional.pos),
                    "name a call from `calls`",
                );
                continue;
            };
            if let crate::call::Role::Called { branch, position } = parsed.role {
                if !call.attempts.iter().any(|a| a.branch == branch && a.position == position) {
                    report.error(
                        "ref/pos-unknown",
                        &path,
                        format!(
                            "position {:?} names no attempt of call {:?}",
                            positional.pos, call.id
                        ),
                        "point at an attempt the call declares",
                    );
                }
            }
        }
    }
}

fn document_refs(index: &Index<'_>, report: &mut Report) {
    if let Some(defect) = &index.pivot.case.defect {
        if !index.steps.contains_key(defect.marker.step.as_str()) {
            report.error(
                "ref/defect-step-unknown",
                "case.defect.marker",
                format!("the marker names {:?}, which is no step", defect.marker.step),
                "mark the step whose outcome IS the defect",
            );
        }
    }
    for deviation in &index.pivot.deviations {
        let path = at("deviations", &deviation.id);
        if let Some(step) = &deviation.step {
            if !index.steps.contains_key(step.as_str()) {
                report.error(
                    "ref/deviation-step-unknown",
                    &path,
                    format!("`step` names {step:?}, which is no step"),
                    "point the deviation at the step it applies to",
                );
            }
        }
        if let Some(leg) = &deviation.leg {
            if !index.legs.contains(leg.as_str()) {
                report.error(
                    "ref/deviation-leg-unknown",
                    &path,
                    format!("`leg` names {leg:?}, which is not declared"),
                    "name a leg from `legs`",
                );
            }
        }
        if let Some(races) = &deviation.races {
            if !index.steps.contains_key(races.as_str()) {
                report.error(
                    "ref/deviation-races-unknown",
                    &path,
                    format!("`races` names {races:?}, which is no step"),
                    "name the step this one races with",
                );
            }
        }
    }
}
