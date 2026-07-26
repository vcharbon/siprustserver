//! Tier-3 admission verdict types, the gate's tunables and seed defaults.
//! The gate itself is [`OverloadSignal::should_admit`](super::OverloadSignal::should_admit).

/// Why the Tier-3 gate rejected a new INVITE. The worker only ever rejects for
/// an empty CPS bucket or the panic-ELU backstop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitReason {
    /// The hard CPS token bucket was empty.
    BucketEmpty,
    /// The worker's own EWMA-ELU exceeded the panic backstop threshold.
    PanicElu,
}

impl AdmitReason {
    /// Short stable tag, keyed by logs and the `reason` metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            AdmitReason::BucketEmpty => "bucket_empty",
            AdmitReason::PanicElu => "panic_elu",
        }
    }
}

/// The Tier-3 admission verdict for one new INVITE.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmitDecision {
    /// `true` → proceed (build the call); `false` → send a stateless 503.
    pub admit: bool,
    /// The rejection reason when `!admit`; `None` on an admit.
    pub reason: Option<AdmitReason>,
    /// Suggested `Retry-After` value (seconds) when `!admit` (0 on an admit).
    pub retry_after_sec: u32,
}

impl AdmitDecision {
    /// An admit verdict (no reason, no Retry-After).
    pub(super) fn admitted() -> Self {
        Self { admit: true, reason: None, retry_after_sec: 0 }
    }
    /// A reject verdict carrying the reason + a Retry-After hint.
    pub(super) fn rejected(reason: AdmitReason, retry_after_sec: u32) -> Self {
        Self { admit: false, reason: Some(reason), retry_after_sec }
    }
}

/// The admission-gate tunables, copied from [`B2buaConfig`](crate::config::B2buaConfig)
/// when the signal is configured. The token bucket capacity/rate are fixed at
/// configure time; the panic threshold + Retry-After base are read live on each
/// [`should_admit`](super::OverloadSignal::should_admit).
#[derive(Debug, Clone, Copy)]
pub(super) struct AdmissionConfig {
    pub(super) panic_elu_threshold: f64,
    pub(super) retry_after_base_sec: u32,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        // Matches `B2buaConfig::default()` so a signal that was never explicitly
        // configured still gates with the standard defaults rather than
        // admitting blind.
        Self {
            panic_elu_threshold: DEFAULT_PANIC_ELU_THRESHOLD,
            retry_after_base_sec: DEFAULT_RETRY_AFTER_BASE_SEC,
        }
    }
}

/// Admission-gate seed defaults — kept in lock-step with the `B2buaConfig`
/// defaults (`b2bua-sdk`), so a publish-only `OverloadSignal::new`/`live` built
/// without config still gates with them. `configure_admission` overwrites these
/// with the operator's settings at ctx build.
pub(super) const DEFAULT_CPS_BUCKET_SIZE: u32 = 1000;
pub(super) const DEFAULT_CPS_BUCKET_RATE: u32 = 500;
pub(super) const DEFAULT_PANIC_ELU_THRESHOLD: f64 = 0.75;
pub(super) const DEFAULT_RETRY_AFTER_BASE_SEC: u32 = 5;

#[cfg(test)]
mod admission_tests {
    use super::*;

    /// The `AdmitReason` tags are stable log/metric keys.
    #[test]
    fn admit_reason_tags_are_stable() {
        assert_eq!(AdmitReason::BucketEmpty.as_str(), "bucket_empty");
        assert_eq!(AdmitReason::PanicElu.as_str(), "panic_elu");
    }
}
