//! The batch's lane-wiring check: a SUT that never called the backend double
//! the batch serves by the time the first cell placing a call ended is wired
//! to another decision service, so every cell after it would fail the same
//! way. The batch names that once and refuses the rest of its cells with it.

use std::sync::Mutex;

/// What the batch knows of its SUT's wiring.
#[derive(Debug, Default)]
pub struct Wiring {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
enum State {
    /// No cell placing a call has ended yet.
    #[default]
    Unjudged,
    /// The SUT called the backend double by then.
    Wired,
    /// It did not: the named error every later cell is refused with.
    Miswired(String),
}

impl Wiring {
    /// The named error, once the batch found its SUT miswired.
    pub fn refusal(&self) -> Option<String> {
        match &*self.state.lock().unwrap_or_else(|p| p.into_inner()) {
            State::Miswired(why) => Some(why.clone()),
            State::Unjudged | State::Wired => None,
        }
    }

    /// A cell ended. The first one that drove the SUT to a verdict on a case
    /// expecting an exchange with the backend double at `backend` (an attempt
    /// to route) judges the wiring off the batch's `exchanges` so far, read
    /// only then; the named error is answered to it where the SUT called
    /// nothing.
    pub fn after_cell(
        &self,
        expects_exchange: bool,
        drove: bool,
        exchanges: impl FnOnce() -> usize,
        ingress: &str,
        backend: &str,
    ) -> Option<String> {
        if !expects_exchange || !drove {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(*state, State::Unjudged) {
            return None;
        }
        if exchanges() > 0 {
            *state = State::Wired;
            return None;
        }
        let why = format!(
            "{MISWIRED} the SUT at {ingress} never called the backend double at {backend} \
             during the batch's first cell; it routes elsewhere"
        );
        *state = State::Miswired(why.clone());
        Some(why)
    }
}

/// How the named error opens: the batch client (`replay-batch.ts` `MISWIRED`)
/// stops sending cells on a cell line whose environment failure opens so.
pub const MISWIRED: &str = "lane wiring:";

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
