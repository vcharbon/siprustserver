//! Reconciling the call map's derived entries with the call map itself, the
//! source of truth (ADR-0051 §7). A store write writes the call map last on an
//! insert and first on a removal, so a panic under the lock leaves derived
//! entries out of step with it; the reaper sweep runs this pass every tick to
//! drop or add them.

use call::call_index_keys;

use super::{CallState, Inner};

/// The corrections one pass made, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    /// Last-touched stamps of no resident call, dropped.
    pub orphan_stamps: u64,
    /// Resident calls with no last-touched stamp, stamped at the pass.
    pub missing_stamps: u64,
    /// Routing-index entries of no resident call, dropped: an owner's key
    /// list, or a key naming a call that is gone.
    pub orphan_index: u64,
    /// Resident calls with no key list, indexed again.
    pub missing_index: u64,
}

impl Reconciled {
    /// Whether the pass corrected nothing.
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }
}

impl CallState {
    /// One reconcile pass at `now_ms`. The map lengths are compared first, and
    /// the entries are walked only when one of them differs from the call
    /// map's: an orphan stamp or index entry is dropped, a resident call with
    /// no stamp is stamped now, one with no key list is indexed again under
    /// the keys no other call holds, and a key a resident call owns is never
    /// moved.
    pub fn reconcile(&self, now_ms: i64) -> Reconciled {
        let mut inner = self.locked();
        let calls = inner.calls.len();
        if inner.touched.len() == calls && inner.indexed.len() == calls {
            return Reconciled::default();
        }
        reconcile_inner(&mut inner, now_ms)
    }

    /// The state an insert a panic cut before its stamp leaves: `call_ref`'s
    /// stamp gone.
    #[cfg(test)]
    pub(crate) fn cut_stamp(&self, call_ref: &str) {
        self.locked().touched.remove(call_ref);
    }

    /// The state a removal a panic cut after its first write leaves:
    /// `call_ref` gone from the call map alone.
    #[cfg(test)]
    pub(crate) fn cut_call(&self, call_ref: &str) {
        self.locked().calls.remove(call_ref);
    }

    /// Whether a routing-index entry names `call_ref`.
    #[cfg(test)]
    pub(crate) fn indexes(&self, call_ref: &str) -> bool {
        let inner = self.locked();
        inner.indexed.contains_key(call_ref) || inner.sip_index.values().any(|r| r == call_ref)
    }
}

fn reconcile_inner(inner: &mut Inner, now_ms: i64) -> Reconciled {
    let mut fixed = Reconciled::default();
    let Inner { calls, touched, indexed, sip_index, .. } = inner;
    touched.retain(|call_ref, _| {
        let resident = calls.contains_key(call_ref);
        fixed.orphan_stamps += u64::from(!resident);
        resident
    });
    indexed.retain(|call_ref, _| {
        let resident = calls.contains_key(call_ref);
        fixed.orphan_index += u64::from(!resident);
        resident
    });
    sip_index.retain(|_, owner| {
        let resident = calls.contains_key(owner);
        fixed.orphan_index += u64::from(!resident);
        resident
    });
    for (call_ref, call) in calls.iter() {
        if !touched.contains_key(call_ref) {
            touched.insert(call_ref.clone(), now_ms);
            fixed.missing_stamps += 1;
        }
        if !indexed.contains_key(call_ref) {
            let keys = call_index_keys(call);
            for key in &keys {
                sip_index.entry(key.clone()).or_insert_with(|| call_ref.clone());
            }
            indexed.insert(call_ref.clone(), keys);
            fixed.missing_index += 1;
        }
    }
    fixed
}

#[cfg(test)]
mod tests;
