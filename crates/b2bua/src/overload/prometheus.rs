//! Prometheus text exposition of the worker-side overload decision INPUTS +
//! DECISIONS (the Tier-3 admission gate + the X-Overload signal).

use super::signal::OverloadSignal;

impl OverloadSignal {
    /// Render the worker-side overload decision INPUTS + DECISIONS as Prometheus
    /// text exposition. Appended to the `/metrics` body by the runner (as the
    /// proxy self-gate's `self_gate_prometheus_text` is). Only the gate's own
    /// inputs and decisions are emitted here; the aggregate reject counter
    /// `b2bua_overload_rejected_total` lives on the core counter set
    /// (`crate::metrics`).
    ///
    /// Series (all `b2bua_overload_*` / `b2bua_emergency_*`; the Grafana
    /// dashboard names are pinned by `b2bua-runner/tests/overload_dashboard_names.rs`):
    ///   - `b2bua_overload_admit_total` (counter) — total new-dialog INVITEs the
    ///     gate admitted = non-emergency `adm` + emergency.
    ///   - `b2bua_overload_reject_total{reason}` (counter) — `bucket_empty` +
    ///     `panic_elu` split.
    ///   - `b2bua_overload_non_emergency_admitted_total` (counter) — the `adm`
    ///     published on X-Overload (kept as its own series; dashboards key on it).
    ///   - `b2bua_emergency_admitted_total` (counter) — emergency-admit volume.
    ///   - `b2bua_overload_token_bucket_level` (gauge) — current CPS bucket level.
    ///   - `b2bua_overload_elu_ewma` / `b2bua_overload_gc_fraction` (gauges) —
    ///     the decision INPUTS (the EWMAs published on X-Overload).
    pub fn prometheus_text(&self) -> String {
        let m = self.metrics();
        let admit_total = m.non_emergency_admitted_total + m.emergency_admitted_total;
        let mut s = String::with_capacity(1024);
        let counter = |s: &mut String, name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
        };
        let gauge = |s: &mut String, name: &str, help: &str, v: f64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
        };
        counter(
            &mut s,
            "b2bua_overload_admit_total",
            "New-dialog INVITEs admitted by the Tier-3 overload gate (non-emergency adm + emergency).",
            admit_total,
        );
        // Per-reason reject split. Pairs with the aggregate
        // b2bua_overload_rejected_total on the core counter set.
        s.push_str("# HELP b2bua_overload_reject_total New-dialog INVITEs shed by the Tier-3 overload gate, by reason.\n");
        s.push_str("# TYPE b2bua_overload_reject_total counter\n");
        s.push_str(&format!(
            "b2bua_overload_reject_total{{reason=\"bucket_empty\"}} {}\n",
            m.reject_bucket_empty_total
        ));
        s.push_str(&format!(
            "b2bua_overload_reject_total{{reason=\"panic_elu\"}} {}\n",
            m.reject_panic_elu_total
        ));
        counter(
            &mut s,
            "b2bua_overload_non_emergency_admitted_total",
            "Non-emergency new-dialog INVITEs admitted (the `adm` counter published on X-Overload).",
            m.non_emergency_admitted_total,
        );
        counter(
            &mut s,
            "b2bua_emergency_admitted_total",
            "Emergency new-dialog INVITEs admitted (always admitted, bypassing the bucket-empty + panic-ELU checks; NOT counted on `adm`).",
            m.emergency_admitted_total,
        );
        gauge(
            &mut s,
            "b2bua_overload_token_bucket_level",
            "Current CPS token-bucket level (tokens remaining; a negative emergency overdraft reads as 0).",
            m.token_bucket_level,
        );
        gauge(
            &mut s,
            "b2bua_overload_elu_ewma",
            "Decision INPUT: EWMA-smoothed Event Loop Utilization (0..1) published on X-Overload; the panic-ELU backstop fires above the threshold.",
            m.elu_ewma,
        );
        gauge(
            &mut s,
            "b2bua_overload_gc_fraction",
            "Decision INPUT: EWMA-smoothed GC pause fraction (0..1) published on X-Overload (structurally 0 on Rust — no managed GC).",
            m.gc_fraction_ewma,
        );
        s
    }
}
