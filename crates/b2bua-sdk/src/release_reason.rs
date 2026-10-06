//! How a deployment restates the RFC 3326 `Reason` on the CANCEL the stack
//! mints toward a pending leg. RFC 3326 lets any value ride a CANCEL, so the
//! default relays the canceller's lines as they came.

/// How the CANCEL this stack mints toward a pending leg restates the `Reason`
/// of the request that asked for it (the caller's CANCEL, or its BYE on an
/// early dialog).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CancelReason {
    /// Every `Reason` line the canceller stated, as it came.
    #[default]
    Verbatim,
    /// The canceller's first value restated alone where it is a Q.850 value,
    /// `Q.850;cause=N` with its cause digits as written; every parameter and
    /// every other value dropped. A first value of another protocol, or one
    /// stating no cause, yields no `Reason`.
    Q850CauseAlone,
}
