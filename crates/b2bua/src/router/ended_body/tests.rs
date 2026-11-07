//! The disposition of an ended body, cause by cause, for a body met
//! `Terminated` and one met `Terminating`.

use call::{Call, CallModelState};

use super::{disposition, Cause, Discharge, Disposition};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

fn met(state: CallModelState) -> Call {
    let config = B2buaConfig::default();
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call = build_initial_call(&invite("w0", "w1", "ended"), src(), &config, &mut ids, 0);
    call.state = state;
    call
}

fn owed(owed_final: bool, limiter_release: bool, discharge: Option<Discharge>) -> Disposition {
    Disposition { owed_final, limiter_release, discharge }
}

#[test]
fn each_cause_is_owed_its_row() {
    use CallModelState::{Terminated, Terminating};
    use Discharge::{AsIs, Forced};
    let rows = [
        (Cause::Departed, Terminated, owed(true, false, Some(AsIs))),
        (Cause::Departed, Terminating, owed(true, false, Some(Forced))),
        (Cause::Reclaim, Terminated, owed(true, false, Some(AsIs))),
        (Cause::Reclaim, Terminating, owed(true, false, Some(Forced))),
        (Cause::ReverseFlush, Terminated, owed(true, false, Some(AsIs))),
        (Cause::ReverseFlush, Terminating, owed(false, false, None)),
        (Cause::ReadDead { still: true }, Terminated, owed(true, true, None)),
        (Cause::ReadDead { still: false }, Terminating, owed(true, false, None)),
        (Cause::Ghost, Terminating, owed(false, false, Some(Forced))),
        (Cause::Evicted, Terminated, owed(false, true, None)),
    ];
    for (cause, state, expected) in rows {
        assert_eq!(disposition(cause, &met(state)), expected, "{cause:?} of a {state:?} body");
    }
}
