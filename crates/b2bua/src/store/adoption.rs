//! Adopting a version another owner of the call published (ADR-0031 D3): the
//! counter taken without that owner's body, and the write that publishes it,
//! which never lands over the other owner's body at that version.

use call::Call;

use super::CallState;

impl CallState {
    /// Raise the resident copy's `(p,b)` to at least `(gen, bak_gen)` and return
    /// it, or `None` when nothing changed (not resident, not replicable, or
    /// already at or past both counters).
    ///
    /// **Not a mutation.** Adopting a version another owner published is a read
    /// this node records, so it carries none of [`update`](Self::update)'s
    /// authoritative bump: bumping our own axis for it would make two live
    /// owners raise each other in turn, one flush per round, and the two views
    /// would never come level (ADR-0031 D3).
    pub fn adopt_version(&self, call_ref: &str, gen: i64, bak_gen: i64) -> Option<Call> {
        let mut inner = self.inner.lock().unwrap();
        let call = inner.calls.get_mut(call_ref)?;
        let t = call.topology.as_mut()?;
        if t.gen >= gen && t.bak_gen >= bak_gen {
            return None;
        }
        t.gen = t.gen.max(gen);
        t.bak_gen = t.bak_gen.max(bak_gen);
        Some((**call).clone())
    }

    /// Flush the adopted copy `call`: as [`flush`](Self::flush), except that a
    /// replicating store already holding the call at that version or past it
    /// keeps what it holds — the body of the owner the version was adopted
    /// from, published at that version.
    pub fn flush_adopted(&self, call: &Call) {
        self.submit_flush(call, self.replicated_ttl_ms, true);
    }
}
