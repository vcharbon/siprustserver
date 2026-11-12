//! What every lane assembles around the interpreter: the actors on their bound
//! agents, the media booking, a panic as text. `bind` and `preflight` hold what
//! a hosted lane's run needs before it dials: endpoints bound on real sockets,
//! and the proof that a loopback path carries a full-size datagram. `seed` is
//! where a cell's ids come from on any lane.

pub mod bind;
pub mod preflight;
pub mod seed;

use std::any::Any;
use std::collections::BTreeMap;
use std::panic::resume_unwind;
use std::path::Path;

use pivot_interpreter::{Booking, Outcome, ReplayError};
use scenario_harness::Agent;

use crate::spec::SdpPlane;
use crate::{Case, Failure};

/// Actor id → the agent playing it, from a bound per-endpoint pool. Two actors
/// sharing an endpoint share the one agent (the loopback shape).
pub fn actors_on_endpoints(
    case: &Case,
    by_endpoint: &BTreeMap<String, Agent>,
) -> Result<BTreeMap<String, Agent>, Failure> {
    let mut agents = BTreeMap::new();
    for actor in &case.document.actors {
        let agent = by_endpoint.get(&actor.endpoint).ok_or_else(|| {
            Failure::Refused(format!(
                "actor {:?} rides endpoint {:?}, which the lane bound no agent for",
                actor.id, actor.endpoint
            ))
        })?;
        agents.insert(actor.id.clone(), agent.clone());
    }
    Ok(agents)
}

/// The media booking a lane's `plane` asks for: none under `verbatim`, so every
/// session description rides as stored; `addr` and RTP ports from `rtp_base`
/// written into `c=`/`m=` under `rewrite`.
pub fn booking(plane: SdpPlane, addr: &str, rtp_base: u16) -> Booking {
    match plane {
        SdpPlane::Verbatim => Booking::verbatim(),
        SdpPlane::Rewrite => Booking::new(addr, rtp_base),
    }
}

/// A panic payload as text, for the summary and the exit path. Callers pass
/// the payload via `.as_ref()`: a `&Box<dyn Any>` unsize-coerces to a
/// `&dyn Any` whose Any IS the box, and every downcast then misses.
pub fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// What the interpreter's `block_on` came to under `catch_unwind`.
pub type Ran = Result<Result<Outcome, ReplayError>, Box<dyn Any + Send>>;

/// One record a lane takes of its cell beside the interpreter's bundle.
pub type Record<'a> = Box<dyn FnOnce() -> Result<(), Failure> + 'a>;

/// The interpreter's outcome, with the cell's `records` taken in order on
/// every path that left a bundle: a run that returned, a bundle that would not
/// write, a panic. The interpreter's own failure stays the one reported, and
/// a record that could not be written on top of it is said; on the returned
/// run the first that fails fails the cell. A plan refusal left no bundle and
/// dialled nothing, so it takes no record. The panic is resumed once the
/// records are taken, so the lane's outer catch reads its own payload.
pub fn recorded(out_dir: &Path, ran: Ran, records: Vec<Record<'_>>) -> Result<Outcome, Failure> {
    match ran {
        Ok(Ok(outcome)) => {
            for record in records {
                record()?;
            }
            Ok(outcome)
        }
        Ok(Err(e @ ReplayError::Plan(_))) => Err(Failure::Refused(e.to_string())),
        Ok(Err(e @ ReplayError::Bundle(_))) => {
            records_or_say(out_dir, records);
            Err(Failure::Environment(e.to_string()))
        }
        Err(panic) => {
            records_or_say(out_dir, records);
            resume_unwind(panic)
        }
    }
}

/// The records behind a failure that is already the cell's: what could not be
/// written is said, never substituted for the run's own error.
fn records_or_say(out_dir: &Path, records: Vec<Record<'_>>) {
    for record in records {
        if let Err(why) = record() {
            crate::diag::line(format_args!(
                "{}: the record was not taken: {why}",
                out_dir.display()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// An assert_eq payload surfaces as its text, not as "non-string panic
    /// payload".
    #[test]
    fn panic_text_reads_an_assert_payload() {
        let payload = catch_unwind(AssertUnwindSafe(|| assert_eq!(1, 2, "call leak"))).unwrap_err();
        let text = panic_text(payload.as_ref());
        assert!(text.contains("call leak"), "got {text:?}");
    }
}
