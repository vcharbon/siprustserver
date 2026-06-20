//! Name-consistency guard for the overload-protection Grafana dashboard.
//!
//! A panel that queries a metric name the exposition does not emit is a *dead*
//! panel — it renders empty forever and nobody notices. This test parses
//! `deploy/observability/stack/grafana/dashboards/overload-protection.json`,
//! extracts every `b2bua_*` / `sip_proxy_*` metric name referenced in a panel
//! `expr`, and asserts each one is actually produced by the migrated overload +
//! emergency exposition:
//!
//!   - `b2bua_*` names are checked against the LIVE renders
//!     ([`OverloadSignal::prometheus_text`] + [`UdpTransportMetrics::prometheus_text`]).
//!   - `sip_proxy_*` names are checked as literals in the proxy exposition
//!     sources (`sip-proxy` + `sip-proxy-runner`) — b2bua-runner does not depend
//!     on `sip-proxy`, so a render here would pull in the whole proxy stack; a
//!     source-literal check is the lighter, equally-load-bearing guard for the
//!     name being wired into *some* exposition (each `sip_proxy_*` series here is
//!     emitted either as a bare `"name"` literal or as a `push_str("…name…")`).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use b2bua::metrics::{B2buaMetrics, UdpTransportMetrics};
use b2bua::overload::{simulated, OverloadSignal};
use b2bua::tier1_brake::Tier1BrakeCounters;

fn repo_path(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/b2bua-runner; the repo root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// Every `b2bua_*` / `sip_proxy_*` token referenced in a panel `expr`.
fn dashboard_metric_names() -> BTreeSet<String> {
    let raw = std::fs::read_to_string(repo_path(
        "deploy/observability/stack/grafana/dashboards/overload-protection.json",
    ))
    .expect("dashboard JSON must exist");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("dashboard must be valid JSON");
    let mut names = BTreeSet::new();
    for panel in json["panels"].as_array().expect("panels array") {
        let Some(targets) = panel["targets"].as_array() else { continue };
        for t in targets {
            let expr = t["expr"].as_str().unwrap_or("");
            for tok in split_metric_tokens(expr) {
                if tok.starts_with("b2bua_") || tok.starts_with("sip_proxy_") {
                    names.insert(tok);
                }
            }
        }
    }
    names
}

/// Pull bare identifier tokens out of a PromQL expr (alnum + `_`).
fn split_metric_tokens(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in expr.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[tokio::test]
async fn every_dashboard_metric_name_is_emitted_by_the_exposition() {
    // Live b2bua exposition: the overload signal + the UDP/Tier-1 brake shape.
    let (sampler, _ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    let brake = Tier1BrakeCounters::new();
    let udp = UdpTransportMetrics::new(
        8,
        brake,
        Arc::new(|| 0),
        Arc::new(|| 0),
    );
    // The aggregate `b2bua_overload_rejected_total` lives on the core counter set.
    let core_metrics = B2buaMetrics::new();
    let b2bua_exposition = format!(
        "{}{}{}",
        sig.prometheus_text(),
        udp.prometheus_text(),
        core_metrics.prometheus_text()
    );

    // Proxy exposition sources (literal check — see module docs).
    let proxy_src = format!(
        "{}{}",
        std::fs::read_to_string(repo_path("crates/sip-proxy/src/observability/metrics.rs"))
            .expect("proxy metrics.rs"),
        std::fs::read_to_string(repo_path("crates/sip-proxy-runner/src/main.rs"))
            .expect("proxy runner main.rs"),
    );

    let mut dead = Vec::new();
    for name in dashboard_metric_names() {
        let found = if name.starts_with("b2bua_") {
            b2bua_exposition.contains(&name)
        } else {
            proxy_src.contains(&name)
        };
        if !found {
            dead.push(name);
        }
    }
    assert!(
        dead.is_empty(),
        "overload-protection.json references metric names no exposition emits (dead panels): {dead:?}"
    );
}
