//! A clean end by the system under test, while the call accepts one
//! ([`CallCtx::accept_clean_ends`](crate::realcall::CallCtx::accept_clean_ends)):
//! the party drops her remaining goals. Once the caller is ended cleanly the
//! call is: its phase barriers are moot, and so are the goals of every party
//! holding no live dialog.

use sip_message::generators::InDialogMethod;
use sip_message::header::{Event, SubscriptionState};
use sip_message::SipRequest;
use tokio::time::Instant;

use super::endpoint::SUBFLOW_REFER;
use super::goals::{FinalAssert, GoalStep, RequestKind};
use super::runner::ActorState;
use super::state::Observation;

/// Whether `notify` ends a subscription this party holds: it reports
/// `Subscription-State: terminated` (RFC 6665 §4.1.3) on the implicit
/// subscription of a REFER she sent and saw accepted (RFC 3515 §2.4.4).
pub(super) fn ends_own_subscription(st: &ActorState<'_>, notify: &SipRequest) -> bool {
    let terminated = notify
        .header::<SubscriptionState>()
        .and_then(Result::ok)
        .is_some_and(|s| s.token().eq_ignore_ascii_case("terminated"));
    let refer = notify
        .header::<Event>()
        .and_then(Result::ok)
        .is_some_and(|e| e.token().eq_ignore_ascii_case("refer"));
    terminated && refer && st.obs.with_snapshot(|s| s.leg(st.role).subflow(SUBFLOW_REFER).is_some())
}

/// What the system under test ended a party with.
#[derive(Clone, Copy)]
pub(super) enum Ending {
    /// A BYE she answered 200.
    Bye,
    /// A NOTIFY ending her subscription.
    Notify,
    /// A final of this status to her establishing INVITE.
    InviteFinal(u16),
}

/// The system under test ended this party cleanly by `ending` — a non-2xx
/// final to her establishing INVITE, a BYE she answered 200, a NOTIFY ending
/// her subscription ([`ends_own_subscription`]). Returns whether the call
/// accepts it: her goals are then dropped, and recorded dropped unless all
/// that is left is the reception `ending` itself is and its answer.
pub(super) fn end_cleanly(st: &mut ActorState<'_>, ending: Ending, now: Instant) -> bool {
    if !st.ctx.clean_ends_accepted() {
        return false;
    }
    st.obs.record(Observation::EndedCleanly { caller: st.originates }, now);
    if !all_met(st.goals.remaining_steps(), ending) {
        st.obs.record(Observation::GoalDropped, now);
    }
    st.goals.abandon();
    true
}

/// Whether the goals `left` are all met by `ending`: none, or the reception
/// the ending is followed by nothing but its answer.
fn all_met<'a>(mut left: impl Iterator<Item = &'a GoalStep>, ending: Ending) -> bool {
    let answer = |step: &GoalStep| {
        matches!(step, GoalStep::Respond { .. } | GoalStep::RespondTemplate { .. })
    };
    left.next().is_none_or(|step| met_by(step, ending)) && left.all(answer)
}

/// Whether `step` is the reception of `ending`, or a hang-up conditioned on
/// an answer the 487 to a CANCELled INVITE rules out (RFC 3261 §9.2): a goal
/// the ending meets rather than drops.
fn met_by(step: &GoalStep, ending: Ending) -> bool {
    let invite = |method: &Option<String>| method.as_deref().is_none_or(|m| m == "INVITE");
    match (step, ending) {
        (GoalStep::ExpectRequest { kind, .. }, Ending::Bye) => {
            *kind == RequestKind::InDialog(InDialogMethod::Bye)
        }
        (GoalStep::ExpectRequest { kind, .. }, Ending::Notify) => {
            *kind == RequestKind::InDialog(InDialogMethod::Notify)
        }
        (GoalStep::ExpectResponse { status: s, cseq_method, .. }, Ending::InviteFinal(status)) => {
            *s == status && invite(cseq_method)
        }
        (GoalStep::ObserveFinal { cseq_method, .. }, Ending::InviteFinal(_)) => invite(cseq_method),
        (GoalStep::ByeIfConfirmed, Ending::InviteFinal(status)) => status == 487,
        (GoalStep::ExpectFinal { assert, cseq_method }, Ending::InviteFinal(status)) => {
            let holds = match *assert {
                FinalAssert::Exact(s) => status == s,
                FinalAssert::Class(c) => status / 100 == c,
                FinalAssert::NonError => status < 400,
            };
            holds && invite(cseq_method)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller whose CANCEL drew its 487 has no dialog to hang up: her
    /// hang-up conditioned on the answer is met, not dropped.
    #[test]
    fn a_non_2xx_final_meets_a_hang_up_conditioned_on_the_answer() {
        assert!(all_met([GoalStep::ByeIfConfirmed].iter(), Ending::InviteFinal(487)));
        assert!(!all_met([GoalStep::ByeIfConfirmed].iter(), Ending::Bye));
        assert!(!all_met([GoalStep::ByeIfConfirmed].iter(), Ending::InviteFinal(500)));
        assert!(!all_met([GoalStep::Bye].iter(), Ending::InviteFinal(487)));
    }
}
