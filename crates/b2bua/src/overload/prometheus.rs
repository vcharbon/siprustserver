//! Prometheus text exposition of the worker-side overload signal: the inputs
//! of the panic-ELU and bucket rungs.

use super::signal::OverloadSignal;
use crate::metrics::catalogue;

impl OverloadSignal {
    /// Render the signal's inputs as Prometheus text exposition, appended to
    /// the `/metrics` body by the runner. Admits and refusals are counted on
    /// `b2bua_new_calls_total` (`crate::new_calls`).
    ///
    /// Series (declared in [`catalogue`]):
    ///   - `b2bua_overload_token_bucket_level` (gauge) — current CPS bucket level.
    ///   - `b2bua_overload_elu_ewma` / `b2bua_overload_gc_fraction` (gauges) —
    ///     the decision inputs (the EWMAs published on X-Overload).
    pub fn prometheus_text(&self) -> String {
        let m = self.metrics();
        let mut s = String::with_capacity(512);
        catalogue::OVERLOAD_TOKEN_BUCKET_LEVEL.render_value(&mut s, m.token_bucket_level);
        catalogue::OVERLOAD_ELU_EWMA.render_value(&mut s, m.elu_ewma);
        catalogue::OVERLOAD_GC_FRACTION.render_value(&mut s, m.gc_fraction_ewma);
        s
    }
}
