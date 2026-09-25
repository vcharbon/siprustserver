//! Pins the capacity gate: each bound's normal and emergency ceilings, the
//! level the ingress brake reads, the backup ceilings, the reject shape and
//! the exposition.

use std::sync::Arc;

use super::*;
use crate::config::{CapacityConfig, Ceilings};

fn gate(limits: CapacityConfig) -> (CapacityGate, SimulatedSystemControl) {
    let (probe, control) = simulated();
    let gate = CapacityGate::new(Arc::new(probe));
    gate.configure(&limits);
    (gate, control)
}

fn occ(calls: u64, transactions: u64) -> Occupancy {
    Occupancy { calls, transactions }
}

fn ceilings(normal: u64, emergency: u64) -> Ceilings {
    Ceilings { normal: Some(normal), emergency: Some(emergency) }
}

#[test]
fn an_unconfigured_gate_admits_everything() {
    let (g, control) = gate(CapacityConfig::default());
    control.set_rss_bytes(Some(u64::MAX - 1));
    g.sample(occ(u64::MAX, u64::MAX));
    assert_eq!(g.level(), Level::Open);
    assert_eq!(g.refuses(false, occ(u64::MAX, u64::MAX)), None);
    assert_eq!(g.refuses_backup(u64::MAX), None);
}

#[test]
fn the_call_ceilings_refuse_normal_then_every_call() {
    let (g, _) = gate(CapacityConfig { calls: ceilings(10, 12), ..Default::default() });
    assert_eq!(g.refuses(false, occ(9, 0)), None);
    assert_eq!(g.refuses(false, occ(10, 0)), Some(Bound::Calls));
    assert_eq!(g.refuses(true, occ(10, 0)), None, "emergency keeps priority");
    assert_eq!(g.refuses(true, occ(11, 0)), None);
    assert_eq!(g.refuses(true, occ(12, 0)), Some(Bound::Calls), "the hard ceiling holds");
}

#[test]
fn the_transaction_ceilings_refuse_normal_then_every_call() {
    let (g, _) = gate(CapacityConfig { transactions: ceilings(100, 150), ..Default::default() });
    assert_eq!(g.refuses(false, occ(0, 99)), None);
    assert_eq!(g.refuses(false, occ(0, 100)), Some(Bound::Transactions));
    assert_eq!(g.refuses(true, occ(0, 149)), None);
    assert_eq!(g.refuses(true, occ(0, 150)), Some(Bound::Transactions));
}

#[test]
fn the_rss_ceilings_read_the_last_sample() {
    let (g, control) =
        gate(CapacityConfig { rss_bytes: ceilings(1000, 2000), ..Default::default() });
    control.set_rss_bytes(Some(1500));
    assert_eq!(g.refuses(false, occ(0, 0)), None, "no sample taken yet");
    g.sample(occ(0, 0));
    assert_eq!(g.rss_bytes(), Some(1500));
    assert_eq!(g.refuses(false, occ(0, 0)), Some(Bound::Rss));
    assert_eq!(g.refuses(true, occ(0, 0)), None);
    control.set_rss_bytes(Some(2000));
    g.sample(occ(0, 0));
    assert_eq!(g.refuses(true, occ(0, 0)), Some(Bound::Rss));
    control.set_rss_bytes(None);
    g.sample(occ(0, 0));
    assert_eq!(g.refuses(true, occ(0, 0)), None, "no reading never trips a bound");
}

#[test]
fn an_emergency_ceiling_alone_bounds_both_classes() {
    let (g, _) = gate(CapacityConfig {
        calls: Ceilings { normal: None, emergency: Some(5) },
        ..Default::default()
    });
    assert_eq!(g.refuses(false, occ(5, 0)), Some(Bound::Calls));
    assert_eq!(g.refuses(true, occ(5, 0)), Some(Bound::Calls));
    assert_eq!(g.refuses(false, occ(4, 0)), None);
}

#[test]
fn a_normal_ceiling_alone_leaves_emergency_unbounded() {
    let (g, _) = gate(CapacityConfig {
        calls: Ceilings { normal: Some(5), emergency: None },
        ..Default::default()
    });
    assert_eq!(g.refuses(false, occ(5, 0)), Some(Bound::Calls));
    assert_eq!(g.refuses(true, occ(1_000_000, 0)), None);
}

#[test]
fn the_level_follows_the_sample() {
    let (g, control) = gate(CapacityConfig {
        calls: ceilings(10, 12),
        rss_bytes: ceilings(1000, 2000),
        ..Default::default()
    });
    g.sample(occ(9, 0));
    assert_eq!(g.level(), Level::Open);
    assert_eq!(g.refuses_at_ingress(false), None);

    g.sample(occ(10, 0));
    assert_eq!(g.level(), Level::ShedNormal);
    assert_eq!(g.refuses_at_ingress(false), Some(Bound::Calls));
    assert_eq!(g.refuses_at_ingress(true), None);

    g.sample(occ(12, 0));
    assert_eq!(g.level(), Level::ShedAll);
    assert_eq!(g.refuses_at_ingress(true), Some(Bound::Calls));

    control.set_rss_bytes(Some(2500));
    g.sample(occ(0, 0));
    assert_eq!(g.refuses_at_ingress(true), Some(Bound::Rss), "names the bound it met");

    control.set_rss_bytes(Some(0));
    g.sample(occ(0, 0));
    assert_eq!(g.level(), Level::Open, "the level reopens once below every ceiling");
}

#[test]
fn the_backup_ceilings_refuse_and_count() {
    let (g, control) = gate(CapacityConfig {
        backup_calls: Some(3),
        backup_rss_bytes: Some(1000),
        ..Default::default()
    });
    assert_eq!(g.refuses_backup(2), None);
    assert_eq!(g.refuses_backup(3), Some(BackupBound::Calls));
    control.set_rss_bytes(Some(1000));
    g.sample(occ(0, 0));
    assert_eq!(g.refuses_backup(0), Some(BackupBound::Rss));
    assert_eq!(g.backup_shed_total(BackupBound::Calls), 1);
    assert_eq!(g.backup_shed_total(BackupBound::Rss), 1);
}

#[test]
fn rejects_are_counted_by_bound_class_and_tier() {
    let (g, _) = gate(CapacityConfig::default());
    g.record_reject(Bound::Calls, false, Tier::Ingress);
    g.record_reject(Bound::Calls, false, Tier::Ingress);
    g.record_reject(Bound::Rss, true, Tier::Admission);
    assert_eq!(g.rejected_total(Bound::Calls, false, Tier::Ingress), 2);
    assert_eq!(g.rejected_total(Bound::Calls, true, Tier::Ingress), 0);
    assert_eq!(g.rejected_total(Bound::Rss, true, Tier::Admission), 1);
    assert_eq!(g.rejected_sum(), 3);
}

#[test]
fn the_exposition_names_every_series() {
    let (g, control) = gate(CapacityConfig {
        calls: ceilings(10, 12),
        backup_calls: Some(7),
        ..Default::default()
    });
    control.set_rss_bytes(Some(4096));
    g.sample(occ(10, 0));
    g.record_reject(Bound::Calls, false, Tier::Admission);
    let txt = g.prometheus_text();
    assert!(txt.contains(
        "b2bua_capacity_rejected_total{bound=\"calls\",class=\"normal\",tier=\"admission\"} 1\n"
    ));
    assert!(txt.contains(
        "b2bua_capacity_rejected_total{bound=\"rss\",class=\"emergency\",tier=\"ingress\"} 0\n"
    ));
    assert!(txt.contains("b2bua_capacity_level 1\n"));
    assert!(txt.contains("b2bua_capacity_rss_bytes 4096\n"));
    assert!(txt.contains("b2bua_capacity_ceiling{bound=\"calls\",class=\"emergency\"} 12\n"));
    assert!(
        !txt.contains("bound=\"transactions\",class=\"normal\"} "),
        "unset ceilings are absent"
    );
    assert!(txt.contains("b2bua_capacity_ceiling{bound=\"backup_calls\",class=\"backup\"} 7\n"));
    assert!(txt.contains("b2bua_repl_backup_shed_total{bound=\"calls\"} 0\n"));
}

mod reject_shape {
    use super::super::build_capacity_reject_503;
    use sip_message::{serialize, SipMessage, SipParser};

    fn invite() -> sip_message::SipRequest {
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-cap\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: capacity@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Content-Length: 0\r\n\r\n";
        match sip_message::CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    #[test]
    fn a_capacity_reject_is_a_tagged_503_with_retry_after_and_no_reason() {
        let resp = build_capacity_reject_503("cap-tag".into(), &invite(), 7);
        let out = String::from_utf8(serialize(&SipMessage::Response(resp))).expect("utf-8");
        assert!(out.starts_with("SIP/2.0 503 Service Unavailable\r\n"), "{out}");
        assert!(out.contains("To: <sip:bob@b2bua.test>;tag=cap-tag\r\n"), "{out}");
        assert!(out.contains("Call-ID: capacity@10.0.0.1\r\n"), "{out}");
        assert!(out.contains("Retry-After: 7\r\n"), "{out}");
        assert!(!out.contains("Reason:"), "{out}");
    }
}
