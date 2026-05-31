//! [`ProxySelfGate`] — proxy self-overload admission gate.
//!
//! **Stubbed in this slice (ADR-0009).** The source `ProxySelfGate.ts` samples
//! the proxy's own event-loop utilization (ELU EWMA) and a CPS token bucket to
//! reject external new-dialog INVITEs under self-overload. That real impl is
//! deferred: overload protection currently relies on (a) OPTIONS-driven worker
//! health/band classification from the B2BUA (the LB filters `above_critical`
//! workers) and (b) `sip-net`'s receive-buffer tail-drop (`PacketQueue`).
//!
//! The seam is kept so the request path's admission branch + the `note_bypass`
//! counters are wired exactly as they will be when the real gate lands — only
//! the decision is hard-coded to admit.

/// The outcome of an admission check (port of `AdmitDecision`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitDecision {
    pub admit: bool,
    /// Set on rejection — the `Reason` phrase (`proxy_overload_elu` /
    /// `proxy_overload_cps`). Always `None` while stubbed.
    pub reason: Option<String>,
    /// `Retry-After` seconds on rejection. `0` while stubbed.
    pub retry_after_sec: u32,
}

impl AdmitDecision {
    pub fn admit() -> Self {
        Self { admit: true, reason: None, retry_after_sec: 0 }
    }
}

/// Why a request bypassed the gate (for the would-be metrics path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassKind {
    Internal,
    Emergency,
}

/// The admission seam. A trait so the real ELU/CPS gate drops in later without
/// touching the request path.
pub trait ProxySelfGate: Send + Sync {
    /// Decide whether to admit an external, non-emergency, new-dialog request.
    fn try_admit_external(&self) -> AdmitDecision;
    /// Note that a request bypassed the gate (internal / emergency).
    fn note_bypass(&self, _kind: BypassKind) {}
}

/// The always-admit stub.
#[derive(Debug, Default, Clone)]
pub struct AlwaysAdmitGate;

impl ProxySelfGate for AlwaysAdmitGate {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision::admit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_always_admits() {
        let g = AlwaysAdmitGate;
        assert!(g.try_admit_external().admit);
        g.note_bypass(BypassKind::Emergency);
    }
}
