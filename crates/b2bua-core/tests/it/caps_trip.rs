//! A turn past a per-call cap trips the same way wherever it runs (ADR-0057
//! §2): the caps read the record's collection lengths and the turn's own
//! actions, so any holder of the same record re-running the event, whether or
//! not another owner served the call, trips on the same cap, its record left
//! as the turn found it.

use b2bua_core::caps::Tripped;
use b2bua_core::config::Cap;
use b2bua_core::turn_driver::{Driver, Outcome, Step};
use sip_message::Method;

use crate::calls::bridged;

#[test]
fn the_same_record_re_running_the_event_trips_the_same_way() {
    let driver = Driver::new().with_config(|c| c.caps.requests = 1);
    let mut first = bridged(&driver);
    first.step(Step::Sends("a", Method::Info));
    let mut served = first.clone();
    served.copy_served(true);
    let before = first.call().clone();

    let tripped = Tripped { cap: Cap::Requests, verdict: false };
    for (who, session) in [("first owner", &mut first), ("second owner", &mut served)] {
        let outcome = session.step(Step::Sends("a", Method::Info));
        assert!(
            matches!(outcome, Outcome::Tripped(t) if *t == tripped),
            "{who}: the second INFO trips the requests' cap: {outcome:?}"
        );
        assert_eq!(session.call(), &before, "{who}: the record stays as the turn found it");
    }
}
