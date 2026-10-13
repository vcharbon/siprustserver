//! Why a per-call dispatch queue was torn down (`b2bua_call_removals_by_class_total`).

/// Why a per-call dispatch queue was torn down: the release that poisoned it.
/// Every removal has exactly one class; they partition `b2bua_call_removals_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalClass {
    /// A call that terminated: it owes its CDR.
    Terminated,
    /// A takeover copy shed by an acting backup (ADR-0014): the call lives on
    /// at its primary, which owes the CDR.
    SelfRelease,
    /// A takeover copy stood down on its primary's write (ADR-0047 D7): the
    /// call lives on at its primary, which owes the CDR.
    StandDown,
    /// A queue that never held a call: an initial INVITE shed statelessly
    /// (overload, store fault) or an event naming no resident call. No CDR.
    Orphan,
}

impl RemovalClass {
    /// Every class, in declaration order.
    pub const ALL: [RemovalClass; 4] = [
        RemovalClass::Terminated,
        RemovalClass::SelfRelease,
        RemovalClass::StandDown,
        RemovalClass::Orphan,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            RemovalClass::Terminated => "terminated",
            RemovalClass::SelfRelease => "self_release",
            RemovalClass::StandDown => "stand_down",
            RemovalClass::Orphan => "orphan",
        }
    }
}
