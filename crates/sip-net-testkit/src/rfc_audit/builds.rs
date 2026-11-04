//! Test-only tally of the projections an audit pass builds, per projection:
//! the observable that bounds how often a pass builds each one.

use std::cell::RefCell;
use std::collections::BTreeMap;

/// Per projection name, how many times it was built.
pub(crate) type Built = BTreeMap<&'static str, usize>;

thread_local! {
    static BUILT: RefCell<Built> = const { RefCell::new(BTreeMap::new()) };
}

/// Count one build of `projection` on this thread.
pub(crate) fn note(projection: &'static str) {
    BUILT.with(|b| *b.borrow_mut().entry(projection).or_default() += 1);
}

/// The builds this thread made since the last take, cleared.
pub(crate) fn take() -> Built {
    BUILT.with(|b| std::mem::take(&mut *b.borrow_mut()))
}
