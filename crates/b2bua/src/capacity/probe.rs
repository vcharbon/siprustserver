//! System-information read seam: the [`SystemProbe`] trait, the production
//! `/proc/self/status` reader, and the injectable [`simulated`] pair for tests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// What the capacity gate reads about the process it runs in. A reading the
/// host cannot produce is `None`, and a bound on it then never trips.
pub trait SystemProbe: Send + Sync {
    /// The process resident set size, in bytes.
    fn rss_bytes(&self) -> Option<u64>;
}

/// Production [`SystemProbe`]: `VmRSS` from `/proc/self/status`. `None` off
/// Linux or when the line cannot be read.
pub(super) struct ProcSelfProbe;

impl SystemProbe for ProcSelfProbe {
    fn rss_bytes(&self) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_vm_rss(&status)
    }
}

/// The `VmRSS:  <n> kB` line of a `/proc/<pid>/status` text, in bytes.
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let mut fields = line["VmRSS:".len()..].split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") => value.checked_mul(1024),
        _ => None,
    }
}

/// Test [`SystemProbe`]: returns whatever its [`SimulatedSystemControl`] last set.
pub struct SimulatedSystemProbe {
    rss: Arc<AtomicU64>,
}

/// Write side of a [`SimulatedSystemProbe`]. Clone-cheap; shares the reading.
#[derive(Clone)]
pub struct SimulatedSystemControl {
    rss: Arc<AtomicU64>,
}

/// `u64::MAX` stands for "no reading".
const NO_READING: u64 = u64::MAX;

impl SimulatedSystemControl {
    /// Another probe reading what this control sets, for a gate built after
    /// the control (a rebooted node keeps its control).
    pub fn probe(&self) -> SimulatedSystemProbe {
        SimulatedSystemProbe { rss: self.rss.clone() }
    }

    /// Set the RSS the probe reports; `None` makes it report no reading.
    pub fn set_rss_bytes(&self, rss: Option<u64>) {
        self.rss.store(rss.unwrap_or(NO_READING), Ordering::Relaxed);
    }
}

impl SystemProbe for SimulatedSystemProbe {
    fn rss_bytes(&self) -> Option<u64> {
        match self.rss.load(Ordering::Relaxed) {
            NO_READING => None,
            v => Some(v),
        }
    }
}

/// A simulated probe and its control, starting at RSS 0.
pub fn simulated() -> (SimulatedSystemProbe, SimulatedSystemControl) {
    let rss = Arc::new(AtomicU64::new(0));
    (SimulatedSystemProbe { rss: rss.clone() }, SimulatedSystemControl { rss })
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn vm_rss_is_read_in_bytes() {
        let status = "Name:\tb2bua\nVmHWM:\t  90000 kB\nVmRSS:\t  81920 kB\nThreads:\t8\n";
        assert_eq!(parse_vm_rss(status), Some(81920 * 1024));
    }

    #[test]
    fn a_status_without_vm_rss_reads_nothing() {
        assert_eq!(parse_vm_rss("Name:\tkthreadd\nThreads:\t1\n"), None);
        assert_eq!(parse_vm_rss("VmRSS:\t12 MB\n"), None);
    }

    #[test]
    fn the_live_probe_reads_this_process() {
        if cfg!(target_os = "linux") {
            assert!(ProcSelfProbe.rss_bytes().is_some_and(|b| b > 0));
        }
    }

    #[test]
    fn the_simulated_probe_reports_what_its_control_set() {
        let (probe, control) = simulated();
        assert_eq!(probe.rss_bytes(), Some(0));
        control.set_rss_bytes(Some(5));
        assert_eq!(probe.rss_bytes(), Some(5));
        control.set_rss_bytes(None);
        assert_eq!(probe.rss_bytes(), None);
    }
}
