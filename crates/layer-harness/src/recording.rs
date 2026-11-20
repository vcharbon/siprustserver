//! Recording helpers (ADR-0004).
//!
//! Boilerplate-elimination for the call shapes a wrappable layer surface
//! actually has. The network surface has two (receiver-style `recv` is
//! recorded inline by the decorator, and the network trait has no sync pure
//! methods):
//!
//!   - [`record_call`]     — an `async` method returning `Result`: record a
//!     `before` event on entry, an `after` event on exit (built from the
//!     outcome).
//!   - [`ReleaseGuard`]    — records an `acquire` event now and a `release`
//!     event on `Drop` (RAII).
//!
//! A layer is also free to record directly via [`crate::Channel::record`] when
//! a helper doesn't fit (higher-order methods, hubs): an explicit wrap with no
//! helper.

use std::future::Future;

use crate::recorder::Channel;

/// Outcome handed to a `record_call` after-builder. A cancelled `Future` is
/// dropped without resuming, so only `Ok` / `Err` are surfaced; a cancelled
/// call simply never records its after-event.
pub enum CallOutcome<'a, T, E> {
    Ok(&'a T),
    Err(&'a E),
}

/// Wrap an async, fallible call so a `before` event fires on entry and an
/// `after` event fires once the inner future resolves. `build_after` may
/// return `None` to skip the after-event for a given outcome (e.g. record only
/// failures). The inner result is returned unchanged.
pub async fn record_call<Evt, T, E, Fut>(
    channel: &Channel<Evt>,
    before: Evt,
    build_after: impl FnOnce(CallOutcome<'_, T, E>) -> Option<Evt>,
    inner: Fut,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
{
    channel.record(before);
    let out = inner.await;
    let after = match &out {
        Ok(v) => build_after(CallOutcome::Ok(v)),
        Err(e) => build_after(CallOutcome::Err(e)),
    };
    if let Some(ev) = after {
        channel.record(ev);
    }
    out
}

/// RAII acquire/release recorder. Construct with [`ReleaseGuard::acquire`] to
/// record the acquire event immediately; the release event records when the
/// guard drops (the scope-close analogue). The wrapped resource typically owns
/// a `ReleaseGuard` so its lifetime and the resource's coincide.
pub struct ReleaseGuard<Evt: Clone> {
    channel: Channel<Evt>,
    release: Option<Evt>,
}

impl<Evt: Clone> ReleaseGuard<Evt> {
    /// Record `acquire` now; arm `release` to fire on drop.
    pub fn acquire(channel: Channel<Evt>, acquire: Evt, release: Evt) -> Self {
        channel.record(acquire);
        Self { channel, release: Some(release) }
    }
}

impl<Evt: Clone> Drop for ReleaseGuard<Evt> {
    fn drop(&mut self) {
        if let Some(ev) = self.release.take() {
            self.channel.record(ev);
        }
    }
}
