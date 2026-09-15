//! Flow well-formedness: what a step may state given its `op`, what an
//! automatic may carry, and the two structural rules that make `alt` and
//! `unordered` safe to interpret.
//!
//! **Alt discriminability is the rule this module exists for.** The interpreter
//! commits to a branch on its first message and never backtracks, so a document
//! whose branches cannot be told apart by that one message does not describe a
//! decidable test — it describes a coin flip. Two branches sharing a first
//! discriminator, or a branch opening on something that may not arrive at all,
//! are both refused structurally rather than discovered at run time.

use std::collections::BTreeSet;

use crate::body::Body;
use crate::flow::{CheckMode, FlowNode, Op, Step};
use crate::lint::transactions::{discharged_by, landing_of};
use crate::lint::{at, reach, Index, Place, Reach, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for (_, step) in index.all_steps() {
        step_rules(step, report);
    }
    in_dialog_marking(index, report);
    confirms_dialog_marking(index, report);
    automatic_body_placement(index, report);
    automatic_ack_has_a_final(index, report);
    for node in &index.pivot.flow {
        match node {
            FlowNode::Alt(alt) => alt_rules(alt, report),
            FlowNode::Unordered(group) => unordered_rules(group, report),
            FlowNode::Inject(inject) => {
                if inject.action.trim().is_empty() {
                    report.error(
                        "inject/action-empty",
                        at("flow", &inject.id),
                        "the injection names no action",
                        "state an action token from the deployment's injector registry",
                    );
                }
            }
            FlowNode::Message(_) => {}
        }
    }
    background(index, report);
    postconditions(index, report);
}

/// `in_dialog` is TOTAL (§6.1): every step of a leg strictly after that leg's
/// dialog-creating final carries it, every step at or before it carries none.
///
/// The marking is what makes §4.1's citation rule checkable, and a method never
/// substitutes for it — an OPTIONS rides a dialog or does not, and only the
/// marker says which — so a leg marked on some of its post-final messages and
/// not on others states nothing at all. Both halves are errors: an unmarked BYE
/// after the 2xx, and a marked message from before the dialog existed.
fn in_dialog_marking(index: &Index<'_>, report: &mut Report) {
    let all: Vec<(Place, &Step)> = index.all_steps().collect();
    for (place, step) in &all {
        let opened = dialog_creating_final(&all, step, *place);
        let expected = !cancel_scoped(step) && opened.is_some();
        if expected == step.in_dialog {
            continue;
        }
        let path = at("flow", &step.id);
        if expected {
            report.error(
                "in-dialog/missing",
                &path,
                format!(
                    "the message runs after leg {:?}'s dialog-creating final ({}) and states no `in_dialog`",
                    step.leg,
                    opened.expect("a final it runs after"),
                ),
                "the marking is total (§6.1): every step after the dialog-creating final carries `in_dialog: true`",
            );
        } else if cancel_scoped(step) {
            report.error(
                "in-dialog/outside-dialog",
                &path,
                "a CANCEL, or a response to one, is marked `in_dialog`",
                "a CANCEL is scoped to the INVITE transaction it cancels (RFC 3261 §9.1) and is never sent within a dialog (§12.2); drop the marker",
            );
        } else {
            report.error(
                "in-dialog/outside-dialog",
                &path,
                format!(
                    "the message is marked `in_dialog`, and no dialog-creating final this step's run reaches precedes it on leg {:?}",
                    step.leg
                ),
                "the marker means CONFIRMED (§6.1); an early-dialog message states its fork with `early` and carries no marker",
            );
        }
    }
}

/// `confirms_dialog` sits on the ACK that answers a dialog-creating final, and
/// on nothing else (§6.1).
///
/// The method cannot say which ACK that is — a re-INVITE's ACK is an ACK inside
/// an established dialog too — and a CSeq chase never substitutes for a marker,
/// so the document STATES which one it is and lint checks the placement:
/// exactly one per confirmed dialog, on an ACK, after the final it answers.
/// Requiredness is about the MARKER, not about the message: a flow carrying no
/// ACK after its dialog-creating final declares no confirming ACK, and §11.1's
/// `no-ack-to-dialog-creating-2xx` is what speaks to an ACK that never came.
fn confirms_dialog_marking(index: &Index<'_>, report: &mut Report) {
    let all: Vec<(Place, &Step)> = index.all_steps().collect();
    let dialogs = dialogs(&all);

    for dialog in &dialogs {
        let Some(ack) = dialog.confirming else { continue };
        if ack.confirms_dialog {
            continue;
        }
        report.error(
            "in-dialog/confirm-missing",
            at("flow", &ack.id),
            format!(
                "the ACK answering leg {:?}'s dialog-creating final ({}) states no `confirms_dialog`",
                ack.leg, dialog.final_id,
            ),
            "the ACK that completes a dialog handshake says so (§6.1); an in-dialog ACK alone does not tell it from a re-INVITE's",
        );
    }

    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for (place, step) in &all {
        if !step.confirms_dialog {
            continue;
        }
        let path = at("flow", &step.id);
        if !is_ack(step) {
            report.error(
                "in-dialog/confirm-not-ack",
                &path,
                "a step that is not an ACK is marked `confirms_dialog`",
                "only the ACK answering a dialog-creating final confirms a dialog (§6.1); drop the marker",
            );
            continue;
        }
        let Some(dialog) = dialog_of(&dialogs, step, *place) else {
            report.error(
                "in-dialog/confirm-outside-dialog",
                &path,
                format!(
                    "the ACK is marked `confirms_dialog`, and no dialog-creating final this step's run reaches precedes it on leg {:?}",
                    step.leg
                ),
                "an ACK to a non-2xx final belongs to the INVITE transaction (RFC 3261 §17.1.1.3) and confirms nothing; drop the marker",
            );
            continue;
        };
        if !claimed.insert(dialog.final_id) {
            report.error(
                "in-dialog/confirm-duplicate",
                &path,
                format!(
                    "a second ACK on leg {:?} is marked `confirms_dialog` for the dialog {} created",
                    step.leg, dialog.final_id,
                ),
                "one dialog is confirmed once (§6.1); mark the ACK that answers the final and no other",
            );
            continue;
        }
        if dialog.confirming.map(|ack| ack.id.as_str()) != Some(step.id.as_str()) {
            report.error(
                "in-dialog/confirm-not-the-answer",
                &path,
                format!(
                    "the ACK is marked `confirms_dialog`, and it is not the one answering leg {:?}'s dialog-creating final ({})",
                    step.leg, dialog.final_id,
                ),
                "an ACK to a re-INVITE's 2xx is an ordinary in-dialog ACK (§6.1); the marker belongs on the ACK the final is answered by",
            );
        }
    }
}

/// One dialog a leg's run opens, and the ACK the flow answers it with.
struct Dialog<'a> {
    place: Place,
    leg: &'a str,
    /// The fork the final answers under, where the document names one.
    fork: Option<&'a str>,
    final_id: &'a str,
    /// Whether the leg carries another dialog on the same run, which is what
    /// makes the fork tag load-bearing for pairing.
    forked: bool,
    /// The ACK that answers the final, where the run carries one.
    confirming: Option<&'a Step>,
}

/// Every dialog the document opens: a `2xx` to an INVITE that no earlier `2xx`
/// to an INVITE on the same leg AND the same fork precedes. Under forking each
/// answered fork mints its own dialog on the one leg, so the fork tag is part of
/// the identity; a re-INVITE's `2xx` names the fork its dialog rings on and so
/// opens nothing.
///
/// The ACK answering the final is the one that DISCHARGES its transaction as
/// the leg's state holds it (`lint::transactions`), never the first ACK the
/// run carries after it: a re-INVITE sent over the un-ACKed 2xx is answered
/// 491 (RFC 3261 §14.1) and its ACK is that transaction's own (§17.1.1.3), so
/// it runs first and confirms nothing.
fn dialogs<'a>(all: &[(Place, &'a Step)]) -> Vec<Dialog<'a>> {
    let creating: Vec<(Place, &'a Step)> =
        all.iter().filter(|(_, step)| is_dialog_creating(step)).copied().collect();
    let opened: Vec<(Place, &'a Step)> = creating
        .iter()
        .filter(|(place, step)| {
            !creating.iter().any(|(other_place, other)| {
                other.id != step.id
                    && other.leg == step.leg
                    && other.early.as_deref() == step.early.as_deref()
                    && reach(*other_place, *place) == Reach::Ok
            })
        })
        .copied()
        .collect();

    opened
        .iter()
        .map(|(place, step)| {
            let forked = opened.iter().any(|(other_place, other)| {
                other.id != step.id
                    && other.leg == step.leg
                    && (reach(*other_place, *place) == Reach::Ok
                        || reach(*place, *other_place) == Reach::Ok)
            });
            let fork = step.early.as_deref();
            let transaction = landing_of(all, step, *place);
            let confirming = all
                .iter()
                .find(|(ack_place, ack)| {
                    is_ack(ack)
                        && ack.leg == step.leg
                        && reach(*place, *ack_place) == Reach::Ok
                        && (!forked || ack.early.as_deref() == fork)
                        && discharged_by(all, ack, *ack_place).is_some_and(|discharge| {
                            discharge.transaction == transaction
                                && is_dialog_creating(discharge.final_)
                        })
                })
                .map(|(_, ack)| *ack);
            Dialog {
                place: *place,
                leg: step.leg.as_str(),
                fork,
                final_id: step.id.as_str(),
                forked,
                confirming,
            }
        })
        .collect()
}

/// The dialog a marked ACK belongs to: the latest one its own run opened on its
/// leg, and under forking the one it names.
fn dialog_of<'a, 'd>(
    dialogs: &'d [Dialog<'a>],
    step: &Step,
    place: Place,
) -> Option<&'d Dialog<'a>> {
    dialogs.iter().rfind(|dialog| {
        dialog.leg == step.leg
            && reach(dialog.place, place) == Reach::Ok
            && (!dialog.forked || dialog.fork == step.early.as_deref())
    })
}

/// Where a transaction-derived message may carry a STORED body (§6.3).
///
/// Three classes can: the ACK to a 2xx, which is where a delayed offer's answer
/// rides (RFC 3261 §13.2.1), and PRACK with its 2xx (RFC 3262 §5). Two cannot,
/// and a payload stored on them would be emitted by nobody: a 100 Trying
/// negotiates nothing, and an ACK to a non-2xx final is absorbed by the INVITE
/// transaction (RFC 3261 §17.1.1.3) and never reaches a TU that could read one.
///
/// An EXPECT composes nothing — its body, a shape or a resource, asserts what
/// arrived (§8.3) — so an expect may state one on any class.
fn automatic_body_placement(index: &Index<'_>, report: &mut Report) {
    let all: Vec<(Place, &Step)> = index.all_steps().collect();
    for (place, step) in &all {
        if !step.auto
            || step.op != Op::Send
            || !matches!(step.msg.body, Some(Body::Resource(_)) | Some(Body::Multipart(_)))
        {
            continue;
        }
        let refusal = if step.msg.status == Some(100) {
            Some("a 100 Trying negotiates nothing")
        } else if is_ack(step) && !acks_a_2xx(&all, step, *place) {
            Some("an ACK to a non-2xx final is absorbed by the INVITE transaction (RFC 3261 §17.1.1.3)")
        } else {
            None
        };
        if let Some(why) = refusal {
            report.error(
                "auto/body-not-composable",
                at("flow", &step.id),
                &format!("a transaction-derived step stores a body the stack cannot place: {why}"),
                "drop the body: the class carries none, so nothing would emit it",
            );
        }
    }
}

/// A transaction-derived ACK is composed FROM the final that answered the
/// INVITE (§6.3: R-URI, Route set, Via and CSeq all come from there), so a flow
/// that ACKs a transaction it never finalises asks for a message nothing can
/// build. The interpreter refuses it at that step; lint says so first.
///
/// The final may sit on ANY leg. A B2BUA relays the one it took, so a leg's
/// answer can reach it on the wire while only the peer leg's step states it —
/// what makes the ACK impossible is no final anywhere between the leg's INVITE
/// and the ACK itself.
fn automatic_ack_has_a_final(index: &Index<'_>, report: &mut Report) {
    let all: Vec<(Place, &Step)> = index.all_steps().collect();
    for (place, step) in &all {
        if !(step.auto && is_ack(step)) {
            continue;
        }
        let Some(opened) = outstanding_invite(&all, step, *place) else { continue };
        let answered = all.iter().any(|(other_place, other)| {
            is_invite_final(other)
                && reach(opened, *other_place) == Reach::Ok
                && reach(*other_place, *place) == Reach::Ok
        });
        if !answered {
            report.error(
                "auto/ack-without-final",
                at("flow", &step.id),
                "a transaction-derived ACK answers a final the flow never states",
                "state the final this ACK answers, or drop the ACK: §6.3 composes it from that transaction",
            );
        }
    }
}

/// Where the INVITE transaction this step's leg has open was opened: the last
/// INVITE request on the leg its own run reaches. RFC 3261 §14.1 leaves one
/// outstanding per dialog, so the latest is the only one.
fn outstanding_invite(all: &[(Place, &Step)], step: &Step, place: Place) -> Option<Place> {
    all.iter()
        .filter(|(other_place, other)| {
            other.leg == step.leg
                && other.msg.status.is_none()
                && other.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
                && reach(*other_place, place) == Reach::Ok
        })
        .next_back()
        .map(|(other_place, _)| *other_place)
}

/// Whether the step is a FINAL answering an INVITE, of any status class.
fn is_invite_final(step: &Step) -> bool {
    step.msg.status.is_some_and(|s| s >= 200)
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

/// Whether this ACK step answers a 2xx: the nearest INVITE final its own run
/// reaches on its leg. A leg holding no final at all reads as non-2xx — the
/// conservative side, since a body nothing emits is worse than one never stored.
fn acks_a_2xx(all: &[(Place, &Step)], step: &Step, place: Place) -> bool {
    all.iter()
        .filter(|(other_place, other)| {
            other.id != step.id
                && other.leg == step.leg
                && other.msg.status.is_some_and(|s| s >= 200)
                && other
                    .msg
                    .cseq_method
                    .as_deref()
                    .is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
                && reach(*other_place, place) == Reach::Ok
        })
        .next_back()
        .is_some_and(|(_, final_)| final_.msg.status.is_some_and(|s| s < 300))
}

/// Whether the step is a `2xx` answering an INVITE.
fn is_dialog_creating(step: &Step) -> bool {
    step.msg.status.is_some_and(|s| (200..300).contains(&s))
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

/// Whether the step is an ACK request.
fn is_ack(step: &Step) -> bool {
    step.msg.status.is_none()
        && step.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("ACK"))
}

/// The id of the dialog-creating final this step's own run reaches: the first
/// `2xx` to an INVITE on its leg that `reach` says has already happened —
/// unconditionally, or inside the same `alt` branch. A branch that took a 487
/// therefore sees no dialog, however the answered branch beside it ends.
fn dialog_creating_final<'a>(
    all: &[(Place, &'a Step)],
    step: &Step,
    place: Place,
) -> Option<&'a str> {
    all.iter()
        .find(|(other_place, other)| {
            other.id != step.id
                && other.leg == step.leg
                && is_dialog_creating(other)
                && reach(*other_place, place) == Reach::Ok
        })
        .map(|(_, other)| other.id.as_str())
}

/// Whether the message belongs to a CANCEL transaction rather than to a dialog.
fn cancel_scoped(step: &Step) -> bool {
    let method = if step.msg.status.is_some() {
        step.msg.cseq_method.as_deref()
    } else {
        step.msg.method.as_deref()
    };
    method.is_some_and(|m| m.eq_ignore_ascii_case("CANCEL"))
}

/// A background policy's settle-time counter has to be checkable: a bound that
/// says nothing, or contradicts itself, would pass every run.
fn background(index: &Index<'_>, report: &mut Report) {
    for actor in &index.pivot.actors {
        let path = at("actors", &actor.id);
        for policy in &actor.background {
            let Some(count) = policy.count else { continue };
            let at_policy = format!("{path}.background[{}]", policy.r#match.method);
            if count == crate::placement::CountBound::default() {
                report.error(
                    "background/count-empty",
                    &at_policy,
                    "`count` states no bound",
                    "state `at_least`, `at_most` or `exactly`, or omit `count` and assert nothing",
                );
            } else if !count.is_satisfiable() {
                report.error(
                    "background/count-unsatisfiable",
                    &at_policy,
                    "the bounds contradict each other",
                    "`exactly` stands alone, and `at_least` never exceeds `at_most`",
                );
            }
        }
    }
}

fn step_rules(step: &Step, report: &mut Report) {
    let path = at("flow", &step.id);
    let is_expect = step.op == Op::Expect;

    if step.msg.method.is_none() && step.msg.status.is_none() {
        report.error(
            "step/discriminator-missing",
            &path,
            "the message states neither a method nor a status",
            "state `method` on a request, `status` plus `cseq-method` on a response",
        );
    }
    if step.check.is_some() != is_expect {
        let (rule, message, hint) = if is_expect {
            (
                "check/missing-on-expect",
                "an `expect` does not say what it does with its content",
                "state `check`: `assert` matches the stored content, `record` stores what arrived",
            )
        } else {
            (
                "check/on-send",
                "a `send` states a `check`",
                "a send is emitted, not checked; drop the field",
            )
        };
        report.error(rule, &path, message, hint);
    }
    if step.optional && !is_expect {
        report.error(
            "optional/on-send",
            &path,
            "a `send` is marked optional",
            "the runner decides when to send; tolerated absence is an `expect` property",
        );
    }
    if step.auto {
        if step.msg.cseq.is_none() {
            report.error(
                "auto/cseq-missing",
                &path,
                "an automatic states no captured CSeq",
                "state `cseq`: without it the automatic pairs with no captured transaction",
            );
        }
        if is_expect && step.check != Some(CheckMode::Record) {
            report.error(
                "auto/not-record",
                &path,
                "an automatic is asserted against",
                "the stack owns the message; an automatic always records",
            );
        }
    } else if step.msg.cseq.is_some() {
        report.error(
            "auto/cseq-on-scripted",
            &path,
            "a scripted step states a captured CSeq",
            "`cseq` is an automatic's pairing token; the interpreter numbers a scripted CSeq itself",
        );
    }
    // §6.3: an ACK rides no retransmission timer, so its count is DRAWN — one
    // ACK per repeat of the final its transaction drew — and a step marking no
    // transaction names no ladder to draw.
    if step.retransmits.is_some_and(|n| n > 0)
        && step.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("ACK"))
        && !step.auto
    {
        report.error(
            "retransmits/scripted-ack",
            &path,
            "a scripted ACK states a retransmission count",
            "an ACK's count is drawn per repeat of the final its transaction drew; mark the step `auto`",
        );
    }
    // §6.9: the stored pacing is one interval per rung, so a count and its
    // intervals state the same ladder or they state two.
    if !step.retransmit_intervals_ms.is_empty()
        && step.retransmit_intervals_ms.len() != step.retransmits.unwrap_or(0) as usize
    {
        report.error(
            "retransmits/intervals-mismatched",
            &path,
            format!(
                "a step declares {} retransmit interval(s) against a count of {}",
                step.retransmit_intervals_ms.len(),
                step.retransmits.unwrap_or(0)
            ),
            "state one interval per rung, or drop them and let the message class pace the ladder",
        );
    }
    // §6.9: a `retransmits` count states the NUMBER and borrows the PACING from
    // the message class, unless the document measured the ladder itself.
    // Measured intervals ARE a stated pacing, so they answer the objection this
    // rule raises and the class no longer has to. An unreliable provisional — a 1xx carrying no `RSeq` —
    // is re-sent at the transaction user's discretion, so RFC 3261 states no
    // interval and RFC 3262 §3 paces only the reliable one. On an `expect` the
    // count is verification and stays legal, whatever draws the copies.
    if step.retransmits.is_some_and(|n| n > 0)
        && !is_expect
        && step.retransmit_intervals_ms.is_empty()
        && step.msg.status.is_some_and(|s| (100..200).contains(&s))
        && !step.msg.headers.iter().any(|h| h.name.eq_ignore_ascii_case("RSeq"))
    {
        report.error(
            "retransmits/unpaced-provisional",
            &path,
            "a sent unreliable provisional states a retransmission count",
            "nothing paces a 1xx without `RSeq`; state one step per emission, each with its own dwell",
        );
    }
    if step.delay.compressible && step.delay.timer_linked {
        report.error(
            "delay/timer-linked-compressed",
            &path,
            "a timer-linked dwell is declared compressible",
            "compressing a dwell a system timer measures changes what the test proves",
        );
    }
    for check in &step.checks {
        if !check.value_is_declarable() {
            report.error(
                "checks/value-mismatch",
                &path,
                format!(
                    "check on {:?} states a value its operator does not take, or omits one it does",
                    check.field
                ),
                "`eq` and `regex` take a value; `exists` and `absent` take none",
            );
        }
    }
}

fn alt_rules(alt: &crate::flow::Alt, report: &mut Report) {
    let path = at("flow", &alt.id);
    if alt.branches.len() < 2 {
        report.error(
            "alt/too-few-branches",
            &path,
            "the alternative offers fewer than two branches",
            "an `alt` with one branch is a sequence; inline it",
        );
    }
    let mut names: BTreeSet<&str> = BTreeSet::new();
    let mut firsts: Vec<(&str, Discriminator<'_>)> = Vec::new();
    for branch in &alt.branches {
        let at_branch = format!("{path}.branches[{}]", branch.name);
        if !names.insert(branch.name.as_str()) {
            report.error(
                "alt/duplicate-branch-name",
                &at_branch,
                format!("branch name {:?} is used twice", branch.name),
                "a later assertion cites a branch by name; make each name unique",
            );
        }
        let Some(first) = branch.steps.first() else {
            report.error(
                "alt/empty-branch",
                &at_branch,
                "the branch has no steps",
                "an empty branch is not an alternative; drop it or give it its messages",
            );
            continue;
        };
        if first.optional {
            report.error(
                "alt/optional-first",
                &at_branch,
                "the branch opens on an optional step",
                "the interpreter commits on the first message; a branch cannot open on one that may not arrive",
            );
        }
        if first.op != Op::Expect {
            report.error(
                "alt/send-first",
                &at_branch,
                "the branch opens on a `send`",
                "which branch runs is decided by what ARRIVES; open on the `expect` that discriminates",
            );
        }
        let mine = Discriminator::of(first);
        for (other, theirs) in &firsts {
            if mine.overlaps(theirs) {
                report.error(
                    "alt/shared-discriminator",
                    &at_branch,
                    format!(
                        "branch {:?} opens on a message branch {other:?} also matches",
                        branch.name
                    ),
                    "the interpreter never backtracks; make the first messages tell the branches apart",
                );
            }
        }
        firsts.push((&branch.name, mine));
    }
}

/// What the interpreter can tell about a branch's first message before it has
/// committed: the leg it arrives on, and the message's own discriminator.
///
/// Two of these OVERLAP when one message can satisfy both — which is not the
/// same as their being equal. A response expectation that states no
/// `cseq-method` matches that status on every transaction, so it overlaps one
/// that names a method, and the pair is undecidable however different the two
/// documents look.
enum Discriminator<'a> {
    Request {
        leg: &'a str,
        method: String,
    },
    Response {
        leg: &'a str,
        status: u16,
        cseq_method: Option<String>,
    },
    /// No discriminator at all; reported by its own rule, and treated here as
    /// matching anything on its leg.
    Undecidable {
        leg: &'a str,
    },
}

impl<'a> Discriminator<'a> {
    fn of(step: &'a Step) -> Self {
        let leg = step.leg.as_str();
        match (&step.msg.method, step.msg.status) {
            (Some(method), _) => Discriminator::Request { leg, method: method.to_uppercase() },
            (None, Some(status)) => Discriminator::Response {
                leg,
                status,
                cseq_method: step.msg.cseq_method.as_ref().map(|m| m.to_uppercase()),
            },
            (None, None) => Discriminator::Undecidable { leg },
        }
    }

    fn leg(&self) -> &str {
        match self {
            Discriminator::Request { leg, .. }
            | Discriminator::Response { leg, .. }
            | Discriminator::Undecidable { leg } => leg,
        }
    }

    fn overlaps(&self, other: &Discriminator<'_>) -> bool {
        if self.leg() != other.leg() {
            return false;
        }
        match (self, other) {
            (Discriminator::Undecidable { .. }, _) | (_, Discriminator::Undecidable { .. }) => true,
            (
                Discriminator::Request { method: mine, .. },
                Discriminator::Request { method: theirs, .. },
            ) => mine == theirs,
            (
                Discriminator::Response { status: mine, cseq_method: my_method, .. },
                Discriminator::Response { status: theirs, cseq_method: their_method, .. },
            ) => {
                mine == theirs
                    && match (my_method, their_method) {
                        // An absent `cseq-method` is a wildcard, not a value.
                        (None, _) | (_, None) => true,
                        (Some(mine), Some(theirs)) => mine == theirs,
                    }
            }
            _ => false,
        }
    }
}

fn unordered_rules(group: &crate::flow::Unordered, report: &mut Report) {
    let path = at("flow", &group.id);
    if group.steps.len() < 2 {
        report.error(
            "unordered/too-few",
            &path,
            "the group holds fewer than two steps",
            "order-freedom between one message and nothing is just a step",
        );
    }
    for step in &group.steps {
        if step.op != Op::Expect {
            report.error(
                "unordered/not-expect",
                at("flow", &step.id),
                "a `send` sits in an `unordered` group",
                "the runner controls when it sends; an order-free group holds the messages it WAITS for",
            );
        }
    }
}

/// The settle contract (§10): a case states what the run must have billed, or
/// says in one greppable token why it cannot.
fn postconditions(index: &Index<'_>, report: &mut Report) {
    match &index.pivot.postconditions {
        None => report.error(
            "postconditions/cdr-absent-needs-reason",
            "postconditions",
            "the document states no postconditions, so nothing checks what the run billed",
            "state `postconditions.cdr.count`, or `postconditions.cdr.absent: \"<reason>\"`",
        ),
        Some(postconditions) => {
            if !postconditions.cdr_is_declared() {
                report.error(
                    "postconditions/cdr-absent-needs-reason",
                    "postconditions.cdr",
                    "no CDR expectation, and no reason given for its absence",
                    "state `cdr.count`, or `cdr.absent: \"<reason>\"` so the gap is greppable",
                );
            }
            let inline = match &postconditions.cdr {
                Some(crate::postcondition::CdrExpectation::Expected(cdr)) => cdr.checks.iter(),
                _ => [].iter(),
            };
            for check in postconditions.checks.iter().chain(inline) {
                if !check.value_is_declarable() {
                    report.error(
                        "checks/value-mismatch",
                        "postconditions",
                        format!("check on {:?} states a value its operator does not take, or omits one it does", check.field),
                        "`eq` and `regex` take a value; `exists` and `absent` take none",
                    );
                }
            }
        }
    }
}
