use std::sync::Arc;

use super::plain::*;
use super::*;
use crate::capacity::{self, CapacityGate};
use crate::dispatch::Discard;
use crate::ingress_brake::IngressBrakeCounters;
use crate::metrics::{B2buaMetrics, UdpTransportMetrics};
use crate::new_calls::{NewCallCounts, NewCallTally, StatelessCounts};
use crate::overload::OverloadSignal;

fn udp() -> UdpTransportMetrics {
    udp_with(IngressBrakeCounters::new())
}

fn udp_with(brake: IngressBrakeCounters) -> UdpTransportMetrics {
    UdpTransportMetrics::new(
        8,
        brake,
        Arc::new(|| 0),
        Arc::new(|| 0),
        Arc::new(|| 0),
        Arc::new(|| 0),
    )
}

fn gate() -> CapacityGate {
    CapacityGate::new(Arc::new(capacity::simulated().0))
}

/// The value of the sample of `family` carrying exactly `labels`.
fn value_of(text: &str, family: &Family, labels: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = labels.iter().map(|(n, v)| format!("{n}=\"{v}\"")).collect();
    let prefix = if pairs.is_empty() {
        format!("{} ", family.name)
    } else {
        format!("{}{{{}}} ", family.name, pairs.join(","))
    };
    let line = text.lines().find(|l| l.starts_with(&prefix));
    line.unwrap_or_else(|| panic!("no sample {prefix:?}"))[prefix.len()..].to_owned()
}

/// Every family this crate renders is in its renderer's text exactly as
/// declared, every declared label set at 0 before any event (the
/// transaction layer's are rendered by the runner).
#[test]
fn every_family_renders_as_declared() {
    let core = B2buaMetrics::new();
    let udp = udp();
    let text = [
        core.prometheus_text(),
        udp.prometheus_text(),
        OverloadSignal::new(Arc::new(load_shed::simulated().0)).prometheus_text(),
        gate().prometheus_text(),
        NewCallCounts::compose(
            core.new_calls(),
            StatelessCounts::default(),
            StatelessCounts::default(),
        )
        .prometheus_text(),
    ]
    .concat();
    for family in
        SECTIONS.iter().flat_map(|s| s.iter()).filter(|f| !TXN.iter().any(|t| t.name == f.name))
    {
        if let Err(mismatch) = family.check(&text) {
            panic!("{mismatch}\n{text}");
        }
    }
}

/// Each label set of a new call carries the count of its own outcome,
/// reason and class.
#[test]
fn a_new_call_series_carries_its_own_outcome_reason_and_class() {
    let tally = NewCallTally::default();
    tally.accept(Class::Emergency);
    (0..5).for_each(|_| tally.cancel(Class::Normal));
    let mut n = 1;
    for reason in Refusal::ALL {
        for class in [Class::Normal, Class::Emergency] {
            n += 1;
            (0..n).for_each(|_| tally.reject(reason, class));
        }
    }
    let backlog = StatelessCounts { refused: [0, 0, 7], copies: 3 };
    let counts = NewCallCounts::compose(&tally, backlog, StatelessCounts::default());
    let text = counts.prometheus_text();
    for class in [Class::Normal, Class::Emergency] {
        let labels = [("outcome", "accepted"), ("class", class.label())];
        let got = value_of(&text, &NEW_CALLS, &labels);
        assert_eq!(got, counts.accepted(class).to_string(), "{labels:?}");
        let labels = [("outcome", "cancelled"), ("class", class.label())];
        let got = value_of(&text, &NEW_CALLS, &labels);
        assert_eq!(got, counts.cancelled(class).to_string(), "{labels:?}");
        for reason in Refusal::ALL {
            let labels =
                [("outcome", "rejected"), ("reason", reason.as_str()), ("class", class.label())];
            let got = value_of(&text, &NEW_CALLS, &labels);
            assert_eq!(got, counts.rejected(reason, class).to_string(), "{labels:?}");
        }
    }
    let labels = [("outcome", "rejected"), ("reason", "deferred_backlog"), ("class", "in_dialog")];
    assert_eq!(value_of(&text, &NEW_CALLS, &labels), "7");
    assert_eq!(value_of(&text, &NEW_CALL_REFUSED_COPIES, &[]), "3");
}

/// Each discard site carries the count of its own answered INVITEs.
#[test]
fn a_discard_site_series_carries_its_own_count() {
    let m = B2buaMetrics::new();
    for (i, why) in DISCARD_SITES.into_iter().enumerate() {
        (0..=i).for_each(|_| m.bump_invite_discard_answered(why));
    }
    let text = m.prometheus_text();
    for why in DISCARD_SITES {
        let labels = [("site", discard_label(why))];
        let got = value_of(&text, &DISPATCH_INVITE_DISCARD_ANSWERED, &labels);
        assert_eq!(got, m.invite_discard_answered_of_total(why).to_string(), "{labels:?}");
    }
    assert_eq!(m.invite_discard_answered_of_total(Discard::Capped), 4);
}

/// Each dispatcher counter carries its own count.
#[test]
fn a_core_refusal_counter_carries_its_own_count() {
    let m = B2buaMetrics::new();
    let bumps: [(&Family, &dyn Fn()); 7] = [
        (&DISPATCH_CAPPED_REFUSALS, &|| m.bump_capped_refusal()),
        (&DISPATCH_CAPPED_REQUEST_ANSWERED, &|| m.bump_capped_request_answered()),
        (&DISPATCH_QUEUE_DROPS, &|| m.bump_queue_drop()),
        (&DISPATCH_CAP_DROPS, &|| m.bump_cap_drop()),
        (&DISPATCH_RELEASE_DISCARDS, &|| m.bump_release_discard()),
        (&DISPATCH_SATURATION, &|| m.bump_saturation()),
        (&DISPATCH_NEW_CALL_SHARE_WAITS, &|| m.bump_new_call_share_wait()),
    ];
    for (n, (_, bump)) in bumps.iter().enumerate() {
        (0..=n).for_each(|_| bump());
    }
    let text = m.prometheus_text();
    for (n, (family, _)) in bumps.iter().enumerate() {
        assert_eq!(value_of(&text, family, &[]), (n + 1).to_string(), "{}", family.name);
    }
}

/// The ingress brake's bypass counter carries its own count.
#[test]
fn the_brake_bypass_counter_carries_its_own_count() {
    let brake = IngressBrakeCounters::new();
    (0..7).for_each(|_| brake.record_emergency_bypass());
    let text = udp_with(brake).prometheus_text();
    assert_eq!(value_of(&text, &UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED, &[]), "7");
}

/// Each overload series carries its own count or reading.
#[tokio::test(start_paused = true)]
async fn an_overload_series_carries_its_own_value() {
    let (sampler, load) = load_shed::simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    sig.configure_admission(&crate::config::B2buaConfig {
        cps_bucket_size: 2,
        cps_bucket_rate: 0,
        ..Default::default()
    });
    load.set_elu(0.9);
    load.set_gc_fraction(0.25);
    sig.sample();
    sig.spend_token();
    let m = sig.metrics();
    assert_ne!(m.elu_ewma, m.gc_fraction_ewma);
    assert_ne!(m.elu_ewma, m.token_bucket_level);
    assert_eq!(m.token_bucket_level, 1.0, "the token spent");
    let text = sig.prometheus_text();
    let level = value_of(&text, &OVERLOAD_TOKEN_BUCKET_LEVEL, &[]);
    assert_eq!(level, m.token_bucket_level.to_string());
    assert_eq!(value_of(&text, &OVERLOAD_ELU_EWMA, &[]), m.elu_ewma.to_string());
    assert_eq!(value_of(&text, &OVERLOAD_GC_FRACTION, &[]), m.gc_fraction_ewma.to_string());
}
