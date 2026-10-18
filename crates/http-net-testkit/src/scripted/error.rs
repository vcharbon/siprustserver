//! Why `add` refuses a script.

/// A script `add` refuses: it could never be served as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpScriptError {
    /// The open or a step names an empty method or path.
    #[error("{at}: empty method or path")]
    EmptyTarget {
        /// Where, e.g. `open` or `step 1 expect`.
        at: String,
    },
    /// A reified script with no step: nothing answers its open.
    #[error("a reified script needs at least one step")]
    NoStep,
    /// A `${…}` outside the closed grammar, or unterminated.
    #[error(
        "{at}: placeholder {text:?} is not ${{bind:NAME}}, ${{capture:NAME}} or ${{continuation}}"
    )]
    UnknownPlaceholder {
        /// Where.
        at: String,
        /// The offending text.
        text: String,
    },
    /// `${continuation}` in a request match: the token is the service's own.
    #[error("{at}: ${{continuation}} belongs in a reply, not a match")]
    ContinuationInMatch {
        /// Where.
        at: String,
    },
    /// `${bind:NAME}` with no binding given.
    #[error("{at}: ${{bind:{name}}} has no binding")]
    UnknownBind {
        /// Where.
        at: String,
        /// The name.
        name: String,
    },
    /// A reply references a capture no earlier match takes.
    #[error("{at}: ${{capture:{name}}} is not captured at or before this step")]
    UnboundCapture {
        /// Where.
        at: String,
        /// The name.
        name: String,
    },
    /// A capture not preceded by literal text in its entry, or taken twice in
    /// one entry: it cannot be located in the body.
    #[error("{at}: ${{capture:{name}}} must follow literal text and appear once per entry")]
    UnanchoredCapture {
        /// Where.
        at: String,
        /// The name.
        name: String,
    },
    /// A step after one whose reply mints no token (`Silence`, `Reset`, or a
    /// response without `${continuation}`): no request can reach it.
    #[error("step {index} is unreachable: step {previous} mints no continuation")]
    UnreachableStep {
        /// The unreachable step.
        index: usize,
        /// The step that mints no token.
        previous: usize,
    },
    /// Step 0 answers the opening request, so its method and path are the
    /// open's.
    #[error("step 0 expects {expected}, the open is {open}")]
    StepZeroTarget {
        /// The open's `method path`.
        open: String,
        /// Step 0's `method path`.
        expected: String,
    },
    /// A status no final response can carry (RFC 9110 §15: 2xx to 5xx).
    #[error("{at}: status {status} is not a final response status (200-599)")]
    InvalidStatus {
        /// Where.
        at: String,
        /// The status.
        status: u16,
    },
    /// A header name that is not a token, or a value with a byte a server
    /// cannot send (RFC 9110 §5.1, §5.5).
    #[error("{at}: header {name:?}: {why}")]
    InvalidHeader {
        /// Where.
        at: String,
        /// The header name.
        name: String,
        /// What is wrong.
        why: String,
    },
}
