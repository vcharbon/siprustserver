//! The call states the driven tests start from, each reached by the events
//! that land it.

use b2bua_core::turn_driver::{Driver, Session, Step};

/// alice's call ringing at bob: his 180 relayed, her INVITE unanswered.
pub(crate) fn ringing(driver: &Driver) -> Session<'_> {
    let mut call = driver.dial();
    call.run([Step::Answers("b-1", 180)]);
    call
}

/// alice's call bridged to bob: his 2xx relayed and ACKed by her.
pub(crate) fn bridged(driver: &Driver) -> Session<'_> {
    let mut call = driver.dial();
    call.run([Step::Answers("b-1", 180), Step::Answers("b-1", 200), Step::AliceAcks]);
    call
}
