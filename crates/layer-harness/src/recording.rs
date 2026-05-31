//! Recording helpers (port of `recordingHelpers.ts`, ADR-0013 D4).
//!
//! Boilerplate-elimination for the call shapes a wrappable layer surface
//! actually has. The TS source had four (`recordSync`, `recordEffectCall`,
//! `recordScopedAcquire`, `recordStreamLifecycle`); the Rust network surface
//! collapses to two, because we dropped `Stream` (receiver-style `recv` is
//! recorded inline by the decorator) and there are no sync pure methods on the
//! network trait:
//!
//!   - [`record_call`]     — an `async` method returning `Result`: record a
//!     `before` event on entry, an `after` event on exit (built from the
//!     outcome). The analogue of `recordEffectCall`.
//!   - [`ReleaseGuard`]    — records an `acquire` event now and a `release`
//!     event on `Drop`. The analogue of `recordScopedAcquire`, using RAII
//!     where the source used an Effect scope finalizer.
//!
//! A layer also free to record directly via [`crate::Channel::record`] when a
//! helper doesn't fit (higher-order methods, hubs) — exactly as the source's
//! "explicit-wrap, no helper" convention prescribes.

use std::future::Future;

use crate::recorder::Channel;

/// Outcome handed to a `record_call` after-builder, mirroring
/// `RecordEffectOutcome`. `Interrupt` has no direct Rust analogue for a plain
/// `Future` (cancellation drops the future without resuming), so only `Ok` /
/// `Err` are surfaced; a cancelled call simply never records its after-event.
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
        Self {
            channel,
            release: Some(release),
        }
    }
}

impl<Evt: Clone> Drop for ReleaseGuard<Evt> {
    fn drop(&mut self) {
        if let Some(ev) = self.release.take() {
            self.channel.record(ev);
        }
    }
}
