//! Process-wide mux counters + their Prometheus rendering.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use sip_message::sniff::{cseq_method_label, cseq_value, first_line};

use super::MuxCore;

/// Process-wide mux counters (Prometheus + report).
#[derive(Default)]
pub struct MuxStats {
    pub orphan_no_header: AtomicU64,
    pub orphan_unknown_token: AtomicU64,
    pub orphan_stray: AtomicU64,
    pub pending_expired: AtomicU64,
    pub inbox_drop: AtomicU64,
    pub delivered: AtomicU64,
    /// Datagrams the per-call loss model deliberately discarded, split by
    /// direction: `out` = never hit the wire (dropped in `send_to`); `in` =
    /// demuxed to the call but discarded before the app read it.
    pub dropped_out: AtomicU64,
    pub dropped_in: AtomicU64,
    sample_cap: usize,
    samples: Mutex<Vec<String>>,
    /// Per-`(reason, CSeq-method)` orphan breakdown, so an orphan burst is
    /// triageable from `/metrics` alone ("stray BYE: N, stray OPTIONS: M")
    /// without a packet capture. Off the hot path (orphans only), so a
    /// `Mutex<map>` is fine.
    orphan_by_method: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
}

impl MuxStats {
    pub(super) fn new(sample_cap: usize) -> Self {
        Self { sample_cap, ..Default::default() }
    }

    pub(super) fn orphan(&self, reason: OrphanReason, raw: &[u8]) {
        match reason {
            OrphanReason::NoHeader => self.orphan_no_header.fetch_add(1, Ordering::Relaxed),
            OrphanReason::UnknownToken | OrphanReason::NoRoute => {
                self.orphan_unknown_token.fetch_add(1, Ordering::Relaxed)
            }
            OrphanReason::Stray => self.orphan_stray.fetch_add(1, Ordering::Relaxed),
        };
        let method = cseq_method_label(raw);
        *self.orphan_by_method.lock().unwrap().entry((reason.label(), method)).or_default() += 1;
        let mut g = self.samples.lock().unwrap();
        if g.len() < self.sample_cap {
            // Lead the sample with the CSeq (method + number) so a sampled orphan is
            // self-describing for troubleshooting, then the request/response line.
            g.push(format!("[{}] {} | {}", reason.label(), cseq_value(raw), first_line(raw)));
        }
    }

    /// Bounded orphan samples (the "notify" surface).
    pub fn samples(&self) -> Vec<String> {
        self.samples.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy)]
pub(super) enum OrphanReason {
    /// An initial INVITE we cannot correlate (no token) — the concerning case.
    NoHeader,
    /// A token present but matching no pending call.
    UnknownToken,
    /// A token matched a call, but its scenario-owned picker chose a label no
    /// registered receiver carries (a scenario routing bug).
    NoRoute,
    /// An unknown Call-ID that is not an initial INVITE (a late straggler).
    Stray,
}

impl OrphanReason {
    fn label(self) -> &'static str {
        match self {
            OrphanReason::NoHeader => "no_header",
            OrphanReason::UnknownToken => "unknown_token",
            OrphanReason::NoRoute => "no_route",
            OrphanReason::Stray => "stray",
        }
    }
}

impl MuxCore {
    /// Render the mux Prometheus series.
    pub fn render_prometheus(&self) -> String {
        let s = self.stats();
        let mut out = String::new();
        // Orphans are labelled by reason AND CSeq method (`sum by(reason)` still
        // aggregates to the per-reason total for existing queries). Always emit the
        // three reason×none zero-series so a fresh run has the series present.
        out.push_str("# HELP loadgen_mux_orphan_total Inbound datagrams that matched no call, by reason and CSeq method.\n");
        out.push_str("# TYPE loadgen_mux_orphan_total counter\n");
        let by = s.orphan_by_method.lock().unwrap();
        if by.is_empty() {
            for r in ["no_header", "unknown_token", "stray"] {
                out.push_str(&format!("loadgen_mux_orphan_total{{reason=\"{r}\",method=\"none\"}} 0\n"));
            }
        } else {
            for ((reason, method), n) in by.iter() {
                out.push_str(&format!(
                    "loadgen_mux_orphan_total{{reason=\"{reason}\",method=\"{method}\"}} {n}\n"
                ));
            }
        }
        drop(by);
        out.push_str("# HELP loadgen_mux_registry_size Live demux entries (leak canary).\n");
        out.push_str("# TYPE loadgen_mux_registry_size gauge\n");
        out.push_str(&format!("loadgen_mux_registry_size {}\n", self.registry_size()));
        out.push_str("# HELP loadgen_mux_pending_expired_total Pending callee legs reaped (never arrived).\n");
        out.push_str("# TYPE loadgen_mux_pending_expired_total counter\n");
        out.push_str(&format!(
            "loadgen_mux_pending_expired_total {}\n",
            s.pending_expired.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP loadgen_mux_inbox_drop_total Datagrams dropped on a full call inbox.\n");
        out.push_str("# TYPE loadgen_mux_inbox_drop_total counter\n");
        out.push_str(&format!("loadgen_mux_inbox_drop_total {}\n", s.inbox_drop.load(Ordering::Relaxed)));
        out.push_str("# HELP loadgen_mux_delivered_total Datagrams demuxed to a call.\n");
        out.push_str("# TYPE loadgen_mux_delivered_total counter\n");
        out.push_str(&format!("loadgen_mux_delivered_total {}\n", s.delivered.load(Ordering::Relaxed)));
        out.push_str("# HELP loadgen_drop_total Datagrams dropped by the simulated packet-loss model, by direction.\n");
        out.push_str("# TYPE loadgen_drop_total counter\n");
        out.push_str(&format!(
            "loadgen_drop_total{{dir=\"out\"}} {}\n",
            s.dropped_out.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "loadgen_drop_total{{dir=\"in\"}} {}\n",
            s.dropped_in.load(Ordering::Relaxed)
        ));
        out
    }
}
