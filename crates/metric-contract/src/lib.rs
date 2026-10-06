//! metric-contract — the metric families every shipped binary serves on
//! `/metrics`, as one contract a reader of the metrics is checked against.
//!
//! - [`catalogues`] — the catalogue of each binary, in a fixed order;
//!   `metric_catalogue::export::to_json` states them as JSON.
//! - [`contract::Contract`] — a contract read back from that JSON.
//! - [`scan`] — the checker: every metric name a text spells under the
//!   families' namespaces, and every label (and fixed label value) of a plain
//!   `{label="…"}` selector attached to a catalogued name, against the
//!   contract and an allow-list of names that are not ours, each scoped to
//!   its files ([`allow`]).

#![forbid(unsafe_code)]

pub mod allow;
pub mod contract;
pub mod scan;

use metric_catalogue::Catalogue;

/// The catalogue of every shipped binary that serves `/metrics`.
pub fn catalogues() -> Vec<Catalogue> {
    vec![
        b2bua_runner_kit::CATALOGUE,
        sip_proxy_runner::metrics_body::CATALOGUE,
        call_limiter::CATALOGUE,
        loadgen::CATALOGUE,
        cdr_consumer_runner::CATALOGUE,
    ]
}

/// The JSON of every catalogue.
pub fn export() -> String {
    metric_catalogue::export::to_json(&catalogues())
}
