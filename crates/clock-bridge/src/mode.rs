//! Which clock the test side runs on.

/// The clock of the side that awaits the real work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMode {
    /// A paused tokio clock (current-thread runtime): a result is delivered
    /// at its virtual instant, the clock held while a due result is late.
    Paused,
    /// The real clock: a result is delivered when it arrives; the awaiting
    /// side owns its own timeouts.
    Real,
}
