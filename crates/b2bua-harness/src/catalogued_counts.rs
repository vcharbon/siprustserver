//! The SUT's admission counters as its `/metrics` body states them.

use std::sync::Arc;

use crate::B2buaSut;

impl B2buaSut {
    /// Every admission counter family
    /// ([`b2bua::metrics::catalogue::ADMISSION`]) the worker renders, each
    /// valued by the sum of its series.
    pub fn catalogued_counts(&self) -> std::collections::BTreeMap<&'static str, f64> {
        let udp = self.ingress_brake().map(|brake| {
            let counters: b2bua::metrics::LiveCounters = Arc::new(Default::default);
            b2bua::metrics::UdpTransportMetrics::new(0, brake.clone(), Arc::new(|| 0), counters)
                .prometheus_text()
        });
        let text = [
            self.metrics.prometheus_text(),
            self.overload().prometheus_text(),
            self.capacity().prometheus_text(),
            self.new_calls().prometheus_text(),
            udp.unwrap_or_default(),
        ]
        .concat();
        b2bua::metrics::catalogue::ADMISSION
            .iter()
            .filter(|family| family.kind.as_str() == "counter")
            .map(|family| {
                let sum = text
                    .lines()
                    .filter(|line| {
                        line.strip_prefix(family.name)
                            .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '))
                    })
                    .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
                    .sum();
                (family.name, sum)
            })
            .collect()
    }
}
