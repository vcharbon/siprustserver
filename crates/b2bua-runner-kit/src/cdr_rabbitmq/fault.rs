//! The panics the sink's own tests inject into its code: a record encoded as
//! one of these payloads panics the code its name says, where nothing of the
//! sink's own panics on its own.

/// The write of a record with this payload panics holding the link, the
/// session it publishes on open.
pub(super) const PANICS_HOLDING_THE_LINK: &[u8] = b"fault: the write panics holding the link";

/// The confirm tracker panics on the record with this payload, once it is
/// the record the tracker resolves.
pub(super) const PANICS_THE_TRACKER: &[u8] = b"fault: the tracker panics resolving this record";

/// Panics when `injected`, saying `at`.
pub(super) fn panic_if(injected: bool, at: &str) {
    if injected {
        panic!("expected: a panic injected {at}");
    }
}
