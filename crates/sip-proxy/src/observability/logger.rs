//! [`ProxyLogger`] — structured routing-decision logging (port of
//! `observability/Logger.ts`). The source emits a Debug-level log per routing
//! decision carrying canonical annotations (`sip.callid`, `sip.method`,
//! `routing.decision`, `routing.strategy`, `worker.target`). Here it is a small
//! trait with a capturing impl (tests) and a no-op default.

use std::sync::Mutex;

/// One routing-decision log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecisionLog {
    pub call_id: String,
    pub method: String,
    pub decision: String,
    pub strategy: String,
    /// `host:port` of the chosen target, if any.
    pub target: Option<String>,
}

/// The logging seam.
pub trait ProxyLogger: Send + Sync {
    fn routing_decision(&self, entry: &RoutingDecisionLog);
}

/// Discards all records (production default until a real sink is wired).
#[derive(Debug, Default, Clone)]
pub struct NoopLogger;

impl ProxyLogger for NoopLogger {
    fn routing_decision(&self, _entry: &RoutingDecisionLog) {}
}

/// Captures records in memory for assertions.
#[derive(Default)]
pub struct CapturingLogger {
    entries: Mutex<Vec<RoutingDecisionLog>>,
}

impl CapturingLogger {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn entries(&self) -> Vec<RoutingDecisionLog> {
        self.entries.lock().unwrap().clone()
    }
}

impl ProxyLogger for CapturingLogger {
    fn routing_decision(&self, entry: &RoutingDecisionLog) {
        self.entries.lock().unwrap().push(entry.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capturing_logger_records_annotations() {
        let log = CapturingLogger::new();
        log.routing_decision(&RoutingDecisionLog {
            call_id: "c1@h".into(),
            method: "INVITE".into(),
            decision: "select_new".into(),
            strategy: "LoadBalancer".into(),
            target: Some("10.0.0.2:5070".into()),
        });
        let e = log.entries();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].decision, "select_new");
        assert_eq!(e[0].target.as_deref(), Some("10.0.0.2:5070"));
    }
}
