//! The fallible-core step vocabulary: [`StepError`] and the panicking
//! [`unwrap_step`] veneer. Every agent primitive is implemented once, fallibly
//! (`try_receive`, `try_expect`, `try_send`, …); the panicking fluent methods
//! (`receive`/`expect`/`send`) are thin veneers over it.

/// A fallible step outcome — the error type of the agent's ONE receive/expect/
/// send core. Two policies, one mechanism: a `#[tokio::test]` stops the world
/// at the first divergence (panic → the panic-time trace dump renders the wire
/// trace), while the load driver *counts* the failure and keeps going (see
/// `loadgen::class`, which matches on the VARIANT — the `Display` text is for
/// humans).
#[derive(Debug, Clone)]
pub enum StepError {
    /// No datagram arrived within the agent's `recv_timeout`.
    Timeout { who: String },
    /// The endpoint's receive queue closed (socket/task gone).
    QueueClosed { who: String },
    /// A datagram arrived but did not parse as SIP.
    Unparseable { who: String, detail: String },
    /// A response arrived with the wrong status code.
    WrongStatus { who: String, expected: u16, got: u16, reason: String },
    /// A request arrived with the wrong method.
    WrongMethod { who: String, expected: String, got: String },
    /// A request arrived where a response was expected (or vice-versa).
    UnexpectedKind { who: String, detail: String },
    /// Sending a datagram failed at the transport.
    Transport { who: String, detail: String },
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Timeout { who } => write!(f, "{who} timed out waiting for a datagram"),
            StepError::QueueClosed { who } => write!(f, "{who} endpoint queue closed"),
            StepError::Unparseable { who, detail } => {
                write!(f, "{who} received an unparseable datagram: {detail}")
            }
            StepError::WrongStatus { who, expected, got, reason } => {
                write!(f, "{who} expected {expected}, got {got} {reason}")
            }
            StepError::WrongMethod { who, expected, got } => {
                write!(f, "{who} expected a {expected} request, got {got}")
            }
            StepError::UnexpectedKind { who, detail } => write!(f, "{who}: {detail}"),
            StepError::Transport { who, detail } => write!(f, "{who} send failed: {detail}"),
        }
    }
}
impl std::error::Error for StepError {}

/// Unwrap a fallible-core step for the panicking (functional-test) lane: `Ok`
/// passes through, `Err` becomes `panic!("{e}")` — the same message the load
/// lane would have counted, now stopping the world instead (the panic-time
/// trace dump renders the wire trace). `#[track_caller]` so the panic reports
/// the veneer's line; it cannot reach the *test's* line yet because
/// `#[track_caller]` on an `async fn` is still a no-op on stable
/// (rust-lang/rust#110011) — no loss, the panicking bodies always reported
/// this crate's lines.
#[track_caller]
pub(super) fn unwrap_step<T>(r: Result<T, StepError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}
