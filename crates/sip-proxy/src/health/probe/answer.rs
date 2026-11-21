//! How one OPTIONS answer reads on the two axes of ADR-0050 §3: the
//! worker's health and its acceptance of new calls.

use sip_message::worker_report::{NewCalls, Readiness};

use crate::registry::{Acceptance, WorkerHealth};

/// What one OPTIONS answer says about a worker (ADR-0050 §3): the health it
/// sets, `None` to leave it, and the acceptance it sets, `None` to keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnswerReading {
    pub health: Option<WorkerHealth>,
    pub acceptance: Option<Acceptance>,
}

/// Read one OPTIONS answer from its status and its `X-Readiness`: a 200 reads
/// `Alive`, accepting new calls unless its report refuses them; a 503 refuses new
/// calls and leaves the health; any other status reads `Alive` and keeps the
/// acceptance.
pub fn read_answer(status: u16, readiness: Option<&Readiness>) -> AnswerReading {
    match status {
        200 => AnswerReading {
            health: Some(WorkerHealth::Alive),
            acceptance: Some(match readiness.map(|r| r.new) {
                Some(NewCalls::Refuse) => Acceptance::Refuse,
                Some(NewCalls::Accept) | None => Acceptance::Accept,
            }),
        },
        503 => AnswerReading { health: None, acceptance: Some(Acceptance::Refuse) },
        _ => AnswerReading { health: Some(WorkerHealth::Alive), acceptance: None },
    }
}
