//! How an endpoint answers the renegotiations it takes on a confirmed dialog
//! (a re-INVITE, RFC 3261 §14.2; an UPDATE, RFC 3311 §5.2): at once with its
//! description, after a hold, or refused with a final the RFCs allow there.
//!
//! A held answer keeps its server transaction open, well inside the peer's
//! Timer B or F (64·T1): the peer re-sends an UPDATE meanwhile, and a re-INVITE
//! stays in progress, so neither side starts another on the dialog (§14.1). A
//! BYE taken while an answer is held draws `487` to the held request
//! (§15.1.2), and the endpoint's own BYE sends what it holds before it leaves.

use std::time::Duration;

use tokio::time::Instant;

use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::Observation;
use crate::{ServerTxn, StepError};

/// What the endpoint does with one kind of renegotiation it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Answer {
    /// How long the final waits after the request arrives.
    pub hold: Duration,
    /// The final: `None` the endpoint's 2xx with its description, else this
    /// non-2xx status (`488`, `500` with `Retry-After`, `491`, …).
    pub refuse: Option<u16>,
}

impl Answer {
    /// The default: a 2xx at once.
    pub fn is_default(&self) -> bool {
        self.hold.is_zero() && self.refuse.is_none()
    }
}

/// How the endpoint answers each renegotiation method; one [`Answer`] serves
/// every request of its method. A caller also holds the PRACK of each
/// reliable provisional it takes for `prack` (the `held_pracks` module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Answers {
    pub reinvite: Answer,
    pub update: Answer,
    pub prack: Duration,
}

/// What an endpoint holds back or refuses: how it answers renegotiations, the
/// finals and PRACKs it holds, and how many finals it ever held.
#[derive(Default)]
pub(super) struct Holds {
    pub(super) answers: Answers,
    pub(super) answered: Vec<Held>,
    pub(super) opened: u32,
    pub(super) pracks: Vec<super::held_pracks::HeldPrack>,
}

impl Holds {
    pub(super) fn new(answers: Answers) -> Self {
        Self { answers, ..Self::default() }
    }
}

/// A renegotiation whose final is held.
pub(super) struct Held {
    pub(super) at: Instant,
    pub(super) key: ObligationKey,
    pub(super) uas: Box<ServerTxn>,
    pub(super) answer: Answer,
}

/// The reason phrase of a refusal.
fn reason(status: u16) -> &'static str {
    match status {
        488 => "Not Acceptable Here",
        491 => "Request Pending",
        500 => "Server Internal Error",
        503 => "Service Unavailable",
        _ => "Refused",
    }
}

/// The [`Answer`] this endpoint gives a confirmed dialog's `method`.
pub(super) fn answer_for(st: &ActorState<'_>, method: &str) -> Answer {
    match method {
        "INVITE" => st.holds.answers.reinvite,
        "UPDATE" => st.holds.answers.update,
        _ => Answer::default(),
    }
}

/// Hold `uas`'s final for `answer.hold`; the obligation keeps the call open
/// until it is sent.
pub(super) fn hold(st: &mut ActorState<'_>, uas: ServerTxn, answer: Answer) {
    let now = Instant::now();
    st.holds.opened += 1;
    let key = ObligationKey::new(st.role, ObligationKind::HeldAnswer, st.holds.opened);
    let detail = format!("{} final held", uas.request().method().as_str());
    st.obs.record(Observation::RequestSent { key, detail }, now);
    st.holds.answered.push(Held { at: now + answer.hold, key, uas: Box::new(uas), answer });
}

/// Send a refusal of `uas`: a re-INVITE's then awaits its hop-ACK.
pub(super) async fn refuse(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
    status: u16,
) -> Result<(), StepError> {
    let mut r = uas.respond(status, reason(status));
    if matches!(status, 500 | 503) {
        r = r.with_header("Retry-After", "1");
    }
    r.try_send().await?;
    if uas.request().method().as_str() == "INVITE" {
        super::answer::arm_reject_final(st, &uas, status);
    }
    Ok(())
}

/// When the earliest held final is due.
pub(super) fn due(st: &ActorState<'_>) -> Option<Instant> {
    st.holds.answered.iter().map(|h| h.at).min()
}

/// Send every held final due by `until` (`None`: every one): its refusal, or
/// the answer the endpoint gives at once.
pub(super) async fn fire(st: &mut ActorState<'_>, until: Option<Instant>) -> Result<(), StepError> {
    let (due, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut st.holds.answered)
        .into_iter()
        .partition(|h| until.is_none_or(|t| h.at <= t));
    st.holds.answered = kept;
    for held in due {
        st.obs.record(Observation::ResponseObserved { key: held.key }, Instant::now());
        match held.answer.refuse {
            Some(status) => refuse(st, *held.uas, status).await?,
            None => super::react::answer_reoffer(st, *held.uas).await?,
        }
    }
    Ok(())
}

/// A BYE ended the dialog: every held request draws `487` (RFC 3261 §15.1.2).
pub(super) async fn terminate(st: &mut ActorState<'_>) -> Result<(), StepError> {
    for held in std::mem::take(&mut st.holds.answered) {
        st.obs.record(Observation::ResponseObserved { key: held.key }, Instant::now());
        let mut uas = *held.uas;
        uas.respond(487, "Request Terminated").try_send().await?;
        if uas.request().method().as_str() == "INVITE" {
            super::answer::arm_reject_final(st, &uas, 487);
        }
    }
    Ok(())
}
