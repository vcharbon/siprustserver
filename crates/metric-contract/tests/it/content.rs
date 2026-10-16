//! A product registers its catalogue on the HA tooling search path
//! (`<entry>/metrics/*.json`): the checker then holds the product's readers
//! to it, and refuses a product metric the catalogue does not declare.

use std::path::{Path, PathBuf};

use metric_contract::allow::Allow;
use metric_contract::content::contract;
use metric_contract::scan::check_paths;

const WIDGET: &str = r#"{"namespaces": ["widget_"], "catalogues": [{"binary": "widget-server", "families": [
  {"name": "widget_requests_total", "kind": "counter", "semi_open": false,
   "labels": [[{"name": "endpoint", "values": ["create", "delete"]}]], "help": "Requests by endpoint."},
  {"name": "widget_latency_seconds", "kind": "histogram", "semi_open": true,
   "labels": [[{"name": "endpoint", "values": []}]], "help": "Request latency."}
]}]}"#;

/// A scratch product: its search-path entry and one reader beside it.
struct Product {
    dir: PathBuf,
}

impl Product {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("metric-contract-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("content/metrics")).expect("content dir");
        Product { dir }
    }

    fn content(&self) -> PathBuf {
        self.dir.join("content")
    }

    fn catalogue(&self, json: &str) {
        std::fs::write(self.content().join("metrics/widget.json"), json).expect("catalogue");
    }

    fn reader(&self, text: &str) -> PathBuf {
        let p = self.dir.join("dashboard.json");
        std::fs::write(&p, text).expect("reader");
        p
    }
}

impl Drop for Product {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn names(contract: &metric_contract::contract::Contract, reader: &Path) -> Vec<String> {
    let findings = check_paths(contract, &Allow::default(), &[reader.to_owned()]);
    findings.iter().map(|f| format!("{}: {}", f.name, f.what)).collect()
}

#[test]
fn the_checker_refuses_a_product_metric_its_catalogue_does_not_declare() {
    let product = Product::new("unregistered");
    product.catalogue(WIDGET);
    let reader = product.reader(
        r#"{"expr": "sum(rate(widget_requests_total{endpoint=\"create\"}[1m])) / sum(widget_errors_total)",
            "q": "histogram_quantile(0.9, rate(widget_latency_seconds_bucket[5m]))"}"#,
    );
    let contract = contract(&metric_contract::export(), &[product.content()]).expect("contract");
    let found = names(&contract, &reader);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("widget_errors_total: "), "{found:?}");
}

/// One registered family: the declared namespace, not the family's own name,
/// says which names are the product's.
#[test]
fn an_unregistered_name_under_a_single_family_s_declared_namespace_is_refused() {
    let product = Product::new("single");
    product.catalogue(
        r#"{"namespaces": ["widget_"], "catalogues": [{"binary": "widget-server", "families": [
  {"name": "widget_requests_total", "kind": "counter", "semi_open": true,
   "labels": [[]], "help": "Requests."}]}]}"#,
    );
    let reader = product.reader("sum(widget_requests_total) + sum(widget_errors_total)");
    let contract = contract(&metric_contract::export(), &[product.content()]).expect("contract");
    let found = names(&contract, &reader);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("widget_errors_total: "), "{found:?}");
}

#[test]
fn a_product_catalogue_without_declared_namespaces_is_refused() {
    let product = Product::new("undeclared");
    product.catalogue(&WIDGET.replace(r#""namespaces": ["widget_"], "#, ""));
    let err = contract(&metric_contract::export(), &[product.content()]).expect_err("refused");
    assert!(err.contains("declares no namespaces"), "{err}");
}

#[test]
fn a_product_family_outside_its_declared_namespaces_is_refused() {
    let product = Product::new("outside");
    product.catalogue(&WIDGET.replace("widget_latency_seconds", "gadget_latency_seconds"));
    let err = contract(&metric_contract::export(), &[product.content()]).expect_err("refused");
    assert!(
        err.contains("gadget_latency_seconds is under none of its declared namespaces"),
        "{err}"
    );
}

#[test]
fn a_closed_label_value_the_product_catalogue_lacks_is_a_finding() {
    let product = Product::new("closed");
    product.catalogue(WIDGET);
    let reader = product.reader(r#"widget_requests_total{endpoint="rename"}"#);
    let contract = contract(&metric_contract::export(), &[product.content()]).expect("contract");
    assert_eq!(names(&contract, &reader).len(), 1);
}

#[test]
fn a_product_family_redefining_a_shipped_one_is_refused() {
    let product = Product::new("redefine");
    product.catalogue(
        &WIDGET
            .replace("widget_requests_total", "b2bua_active_calls")
            .replace(r#"["widget_"]"#, r#"["widget_", "b2bua_"]"#),
    );
    let err = contract(&metric_contract::export(), &[product.content()]).expect_err("refused");
    assert!(err.ends_with("family b2bua_active_calls is already in the contract"), "{err}");
}

#[test]
fn a_content_directory_that_does_not_exist_is_refused() {
    let err = contract(&metric_contract::export(), &[PathBuf::from("/nonexistent/hakit")])
        .expect_err("refused");
    assert!(err.contains("content directory not found"), "{err}");
}
