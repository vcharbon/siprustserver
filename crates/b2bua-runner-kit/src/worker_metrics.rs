//! The worker's `/metrics` body: every metric source of a worker, rendered
//! in one fixed order.

use b2bua::capacity::CapacityGate;
use b2bua::metrics::{catalogue, B2buaMetrics, UdpTransportMetrics};
use b2bua::new_calls::NewCallCounts;
use b2bua::overload::OverloadSignal;
use metric_catalogue::Catalogue;

/// The metric sources of one worker. Clone-cheap: every field shares its
/// counters with the worker that feeds it.
#[derive(Clone)]
pub struct WorkerMetrics {
    /// The core counter set.
    pub core: B2buaMetrics,
    /// The transaction layer's counters.
    pub txn: sip_txn::TransactionMetrics,
    /// The UDP transport and its ingress brake.
    pub udp: UdpTransportMetrics,
    /// The overload signal: the panic-ELU and bucket rungs' inputs, and
    /// X-Overload.
    pub overload: OverloadSignal,
    /// The capacity gate.
    pub capacity: CapacityGate,
}

/// The b2bua worker's `/metrics` catalogue: every family of
/// [`WorkerMetrics::body`] with the allocator's exposition as its extra, in
/// exposition order.
pub const CATALOGUE: Catalogue = Catalogue {
    binary: "b2bua-runner",
    sections: &[
        catalogue::WORKER,
        catalogue::TXN,
        catalogue::UDP,
        catalogue::OVERLOAD,
        catalogue::CAPACITY,
        catalogue::NEW_CALL_OUTCOMES,
        observe::counters::FAMILIES,
        http_net::failures::FAMILIES,
        jemalloc_stats::catalogue::FAMILIES,
    ],
};

impl WorkerMetrics {
    /// The `/metrics` body: the worker's families, the process-wide
    /// observability counters (dropped log lines, trace admission), the
    /// cause-labelled client HTTP failures, then `extra` (the allocator's
    /// exposition).
    pub fn body(&self, extra: Option<&probe_http::MetricsFn>) -> String {
        let mut text = self.prometheus_text();
        text.push_str(&observe::counters::prometheus_text());
        text.push_str(&http_net::failures::prometheus_text());
        if let Some(extra) = extra {
            text.push_str(&extra());
        }
        text
    }

    /// The worker's families as Prometheus text: core registry, txn
    /// backpressure, UDP transport, overload signal, capacity gate, then every
    /// new call's admission outcome, every rung composed.
    pub fn prometheus_text(&self) -> String {
        let mut text = self.core.prometheus_text();
        text.push_str(&txn_metrics_text(&self.txn));
        text.push_str(&self.udp.prometheus_text());
        text.push_str(&self.overload.prometheus_text());
        text.push_str(&self.capacity.prometheus_text());
        text.push_str(
            &NewCallCounts::read(self.core.new_calls(), &self.txn, Some(self.udp.brake()))
                .prometheus_text(),
        );
        text
    }
}

/// Prometheus text for the transaction layer's families: events-channel
/// depth and capacity, per-class drops and deferrals, the deferred backlog,
/// sweep and forget counts, active transactions, the datagrams it could not
/// parse or send, and the transaction-ladder rungs. The `reason="response"` drop series is the keepalive-response
/// shedding that tears down established dialogs under a new-call burst.
pub fn txn_metrics_text(m: &sip_txn::TransactionMetrics) -> String {
    use sip_txn::catalogue as c;
    use sip_txn::EventQueueClass;
    let mut s = String::new();
    c::ACTIVE_TRANSACTIONS.render_value(&mut s, m.active_transactions());
    c::TIMER_QUEUE_LEN.render_value(&mut s, m.timer_queue_len());
    c::RETRANSMIT_BUF_BYTES.render_value(&mut s, m.retransmit_buf_bytes());
    c::SERVER_FINAL_UNSEEN_BRANCH.render_value(&mut s, m.server_final_unseen_branch());
    c::PARSE_ERRORS.render_value(&mut s, m.parse_errors());
    c::SEND_ERRORS.render_value(&mut s, m.send_errors());
    c::EVENT_QUEUE_DEPTH.render_value(&mut s, m.event_queue_depth());
    c::EVENT_QUEUE_CAPACITY.render_value(&mut s, m.event_queue_capacity());
    let class = |series: &metric_catalogue::Series<'_>| {
        EventQueueClass::ALL[series.index(&c::EVENT_QUEUE_CLASS)]
    };
    c::EVENT_QUEUE_DROPS.render(&mut s, |series| m.event_queue_drops(class(series)));
    c::EVENT_QUEUE_DEFERRALS.render(&mut s, |series| m.event_queue_deferrals(class(series)));
    c::EVENT_QUEUE_DEFERRED.render_value(&mut s, m.event_queue_deferred());
    c::DEFERRED_SWEPT.render_value(&mut s, m.deferred_swept());
    c::SWEEP_REAPED.render_value(&mut s, m.sweep_reaped());
    c::UNANSWERED_FORGOTTEN.render_value(&mut s, m.unanswered_forgotten());
    c::FORGET_REFUSED.render_value(&mut s, m.forget_refused());
    c::RELEASED_UNANSWERED_INVITES_ANSWERED
        .render_value(&mut s, m.released_unanswered_invites_answered());
    c::RELEASED_UNANSWERED_FORGOTTEN.render_value(&mut s, m.released_unanswered_forgotten());
    let rows = m.retransmit_rows().into_iter().map(|row| {
        let mut labels = vec![row.ladder.to_owned(), row.method];
        labels.extend(row.code.map(|code| code.to_string()));
        (labels, row.count)
    });
    c::RETRANSMITS.render_rows(&mut s, rows);
    c::RETRANSMITS_OVERFLOW.render_value(&mut s, m.retransmit_rows_overflowed());
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use b2bua::ingress_brake::IngressBrakeCounters;

    use super::*;

    async fn txn_layer() -> sip_txn::TransactionLayer {
        use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork};
        let net = SimulatedSignalingNetwork::new(1);
        let endpoint = net
            .bind_udp(BindUdpOpts::new("127.0.0.1:5070".parse().unwrap(), 64))
            .await
            .expect("bind");
        let parser = Arc::new(sip_message::CustomParser::new());
        let (txn, _events) = sip_txn::TransactionLayer::spawn(
            endpoint,
            parser,
            sip_txn::TransactionConfig::default(),
        );
        txn
    }

    fn worker(txn: &sip_txn::TransactionLayer) -> WorkerMetrics {
        let (sampler, _load) = load_shed::simulated();
        let (probe, _system) = b2bua::capacity::simulated();
        let udp = UdpTransportMetrics::new(
            8,
            IngressBrakeCounters::new(),
            Arc::new(|| 0),
            Arc::new(|| 0),
            Arc::new(|| 0),
            Arc::new(|| 0),
        );
        WorkerMetrics {
            core: B2buaMetrics::new(),
            txn: txn.metrics().clone(),
            udp,
            overload: OverloadSignal::new(Arc::new(sampler)),
            capacity: CapacityGate::new(Arc::new(probe)),
        }
    }

    /// The worker's body holds every family of its catalogue exactly as
    /// declared, in catalogue order, every declared label set at 0 before any
    /// event, and no family the catalogue does not declare.
    #[tokio::test]
    async fn the_body_holds_its_catalogue_exactly() {
        let txn = txn_layer().await;
        let jemalloc: probe_http::MetricsFn = Arc::new(jemalloc_stats::prometheus_text);
        let text = worker(&txn).body(Some(&jemalloc));
        if let Err(mismatches) = CATALOGUE.check(&text) {
            panic!("{mismatches:#?}\n{text}");
        }
    }

    /// Undeclared label sets of the semi-open families keep the body
    /// conforming: an extension method, a peer, a non-standard status.
    #[tokio::test]
    async fn observed_undeclared_label_sets_keep_the_body_conforming() {
        let txn = txn_layer().await;
        let w = worker(&txn);
        w.core.record_request("FOO");
        w.core.record_response("INVITE", 299);
        w.core.record_repl_applied("backup", "2", "create");
        w.core.record_retransmit("trigger", "INFO", Some(200));
        w.core.record_peer_failure(
            &"192.0.2.9:5060".parse().unwrap(),
            b2bua::peer_failures::PeerScope::External,
            b2bua::peer_failures::PeerFailureKind::SendFailure,
        );
        let text = w.body(None);
        for line in [
            "b2bua_requests_total{method=\"FOO\"} 1",
            "b2bua_responses_total{method=\"INVITE\",code=\"299\"} 1",
            "b2bua_repl_applied_total{flow=\"backup\",peer=\"2\",op=\"create\"} 1",
            "b2bua_retransmits_total{ladder=\"trigger\",method=\"INFO\",code=\"200\"} 1",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line:?} in:\n{text}");
        }
        for family in CATALOGUE.families().filter(|f| !f.name.starts_with("jemalloc_")) {
            if family.name.starts_with("process_") {
                continue;
            }
            if let Err(mismatch) = family.check(&text) {
                panic!("{mismatch}\n{text}");
            }
        }
    }

    /// The deferred-backlog and lifetime series of the transaction layer are
    /// on the first scrape, at 0, so a dashboard or probe reads a value from
    /// startup.
    #[tokio::test]
    async fn the_txn_deferred_backlog_series_are_published_at_zero_from_startup() {
        let txn = txn_layer().await;
        let text = txn_metrics_text(txn.metrics());
        for line in [
            "b2bua_txn_event_queue_deferred 0",
            "b2bua_txn_event_queue_deferred_total{reason=\"request_invite\"} 0",
            "b2bua_txn_event_queue_drops_total{reason=\"request_invite\"} 0",
            "b2bua_txn_deferred_swept_total 0",
            "b2bua_txn_sweep_reaped_total 0",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line:?} in:\n{text}");
        }
        assert!(
            !text.contains("b2bua_txn_deferred_refused_total"),
            "a backlog refusal is a new-call count"
        );
    }
}
