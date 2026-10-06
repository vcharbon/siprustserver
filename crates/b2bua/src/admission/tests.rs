//! Pins the ladder as a table: the order of the rungs, and for each rung and
//! class the input at which it refuses, the reason it states and the
//! `Retry-After` base it raises.

use std::cell::RefCell;
use std::sync::Arc;

use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};

use super::*;
use crate::capacity::{simulated, CapacityGate, CapacityReading, Occupancy};
use crate::config::{CapacityConfig, Ceilings};
use crate::dispatch::{DispatchBody, DispatchClass, PerCallDispatcher};
use crate::metrics::B2buaMetrics;
use crate::new_calls::Refusal;

const CLASSES: [Class; 3] = [Class::Normal, Class::Emergency, Class::InDialog];

fn refused(rung: Rung, class: Class) -> Option<Refused> {
    match judge(rung, class) {
        Verdict::Admit => None,
        Verdict::Refuse(refused) => Some(refused),
    }
}

fn reason(rung: Rung, class: Class) -> Option<Refusal> {
    refused(rung, class).map(|r| r.reason)
}

/// The rungs, in the order a new INVITE meets them.
#[test]
fn the_ladder_orders_the_rungs() {
    assert_eq!(
        LADDER,
        [Step::Brake, Step::Backlog, Step::Capacity, Step::Shed, Step::PanicElu, Step::Bucket]
    );
}

/// The brake refuses a normal INVITE at its threshold and nothing else.
#[test]
fn the_brake_row() {
    let at = |depth| Rung::Brake { depth, threshold: 10 };
    assert_eq!(reason(at(9), Class::Normal), None);
    assert_eq!(reason(at(10), Class::Normal), Some(Refusal::IngressBrake));
    for class in [Class::Emergency, Class::InDialog] {
        assert_eq!(reason(at(usize::MAX), class), None, "{class:?}");
    }
}

/// The backlog refuses a normal INVITE at the normal ceiling, an emergency
/// and an in-dialog one at the emergency ceiling, read as at least the
/// normal one.
#[test]
fn the_backlog_row() {
    let refuses =
        |deferred, normal, emergency, class| ceilings(normal, emergency).refuses(class, deferred);
    assert!(!refuses(7, 8, 12, Class::Normal));
    assert!(refuses(8, 8, 12, Class::Normal));
    for class in [Class::Emergency, Class::InDialog] {
        assert!(!refuses(11, 8, 12, class), "{class:?}");
        assert!(refuses(12, 8, 12, class), "{class:?}");
        assert!(refuses(8, 8, 4, class), "{class:?}");
    }
}

fn capacity(calls: u64, transactions: u64, rss: Option<u64>) -> Rung {
    Rung::Capacity(reading(calls, transactions, rss))
}

fn reading(calls: u64, transactions: u64, rss: Option<u64>) -> CapacityReading {
    let pair = |n, e| Ceilings { normal: Some(n), emergency: Some(e) };
    CapacityReading {
        limits: CapacityConfig {
            calls: pair(10, 12),
            transactions: pair(100, 150),
            rss_bytes: pair(1_000, 2_000),
            ..Default::default()
        },
        occupancy: Occupancy { calls, transactions },
        rss,
    }
}

/// Capacity refuses at each bound's ceiling for the class, the first bound
/// reached naming the reason; an in-dialog INVITE is never refused here.
#[test]
fn the_capacity_row() {
    assert_eq!(reason(capacity(9, 99, Some(999)), Class::Normal), None);
    assert_eq!(reason(capacity(10, 0, None), Class::Normal), Some(Refusal::CapacityCalls));
    assert_eq!(reason(capacity(0, 100, None), Class::Normal), Some(Refusal::CapacityTransactions));
    assert_eq!(reason(capacity(0, 0, Some(1_000)), Class::Normal), Some(Refusal::CapacityRss));
    assert_eq!(
        reason(capacity(10, 100, Some(1_000)), Class::Normal),
        Some(Refusal::CapacityCalls),
        "calls, then transactions, then RSS"
    );
    assert_eq!(reason(capacity(11, 149, Some(1_999)), Class::Emergency), None);
    assert_eq!(reason(capacity(12, 0, None), Class::Emergency), Some(Refusal::CapacityCalls));
    assert_eq!(
        reason(capacity(0, 150, None), Class::Emergency),
        Some(Refusal::CapacityTransactions)
    );
    assert_eq!(reason(capacity(0, 0, Some(2_000)), Class::Emergency), Some(Refusal::CapacityRss));
    assert_eq!(reason(capacity(u64::MAX, u64::MAX, Some(u64::MAX)), Class::InDialog), None);
}

/// The shed refuses whatever the dispatcher found at its threshold; the
/// threshold of each class is its dispatch row's: the cap less the new-call
/// headroom for a normal INVITE, the full cap for an emergency one.
#[tokio::test]
async fn the_shed_row() {
    assert_eq!(reason(Rung::Shed { at_threshold: false }, Class::Normal), None);
    for class in [Class::Normal, Class::Emergency] {
        assert_eq!(reason(Rung::Shed { at_threshold: true }, class), Some(Refusal::CapShed));
    }
    assert_eq!(reason(Rung::Shed { at_threshold: true }, Class::InDialog), None);

    // Cap 4, headroom 1: three live queues refuse a normal INVITE and admit
    // an emergency one; four refuse both.
    let d = PerCallDispatcher::<DispatchBody>::new(8, 8, 4, B2buaMetrics::new())
        .with_new_call_bounds(8, 1);
    let hold = Arc::new(tokio::sync::Notify::new());
    let open = |call: &str| {
        let hold = hold.clone();
        let body: DispatchBody = Box::pin(async move { hold.notified().await });
        let _ = d.offer(call, body, DispatchClass::OtherRequest);
    };
    (0..3).for_each(|i| open(&format!("live-{i}")));
    assert!(d.at_threshold("new", DispatchClass::InitialInvite));
    assert!(!d.at_threshold("new", DispatchClass::EmergencyInvite));
    assert!(!d.at_threshold("live-0", DispatchClass::InitialInvite), "its queue is open");
    open("live-3");
    assert!(d.at_threshold("new", DispatchClass::EmergencyInvite));
    hold.notify_waiters();
}

/// The panic backstop refuses a normal INVITE above its threshold.
#[test]
fn the_panic_elu_row() {
    let at = |elu| Rung::PanicElu { elu, threshold: 0.75 };
    assert_eq!(reason(at(0.75), Class::Normal), None);
    assert_eq!(reason(at(0.76), Class::Normal), Some(Refusal::PanicElu));
    for class in [Class::Emergency, Class::InDialog] {
        assert_eq!(reason(at(1.0), class), None, "{class:?}");
    }
}

/// The bucket refuses a normal INVITE with no token, raising the
/// `Retry-After` base to the time to the next one.
#[test]
fn the_bucket_row() {
    assert_eq!(refused(Rung::Bucket { wait_sec: 0 }, Class::Normal), None);
    assert_eq!(
        refused(Rung::Bucket { wait_sec: 3 }, Class::Normal),
        Some(Refused { reason: Refusal::BucketEmpty, not_before_sec: 3 })
    );
    for class in [Class::Emergency, Class::InDialog] {
        assert_eq!(refused(Rung::Bucket { wait_sec: 60 }, class), None, "{class:?}");
    }
}

/// Every rung but the bucket states the configured base alone.
#[test]
fn only_the_bucket_raises_the_retry_after_base() {
    let refusing = [
        Rung::Brake { depth: 1, threshold: 0 },
        capacity(10, 0, None),
        Rung::Shed { at_threshold: true },
        Rung::PanicElu { elu: 1.0, threshold: 0.5 },
    ];
    for rung in refusing {
        let r = refused(rung, Class::Normal).expect("refused");
        assert_eq!(r.not_before_sec, 0, "{rung:?}");
    }
}

/// Router readings in which every rung refuses but the `admitting` ones,
/// recording which rungs were read.
struct Readings {
    admitting: Vec<Step>,
    read: RefCell<Vec<Step>>,
}

impl Readings {
    fn admitting(steps: &[Step]) -> Self {
        Self { admitting: steps.to_vec(), read: RefCell::new(Vec::new()) }
    }

    fn admits(&self, step: Step) -> bool {
        self.read.borrow_mut().push(step);
        self.admitting.contains(&step)
    }
}

impl RouterReadings for Readings {
    fn capacity(&self) -> CapacityReading {
        reading(if self.admits(Step::Capacity) { 0 } else { 10 }, 0, None)
    }
    fn at_threshold(&self) -> bool {
        !self.admits(Step::Shed)
    }
    fn panic_elu(&self) -> (f64, f64) {
        (if self.admits(Step::PanicElu) { 0.0 } else { 1.0 }, 0.5)
    }
    fn token_wait_sec(&self) -> u32 {
        if self.admits(Step::Bucket) {
            0
        } else {
            5
        }
    }
}

/// At router ingress the first refusing rung, in ladder order, names the
/// refusal: capacity before the shed, the shed before the panic backstop,
/// the panic backstop before the bucket.
#[test]
fn the_router_judges_its_rungs_in_ladder_order() {
    let first = |admitting: &[Step]| {
        first_refusal(Class::Normal, &Readings::admitting(admitting)).map(|r| r.reason)
    };
    assert_eq!(first(&[]), Some(Refusal::CapacityCalls));
    assert_eq!(first(&[Step::Capacity]), Some(Refusal::CapShed));
    assert_eq!(first(&[Step::Capacity, Step::Shed]), Some(Refusal::PanicElu));
    assert_eq!(first(&[Step::Capacity, Step::Shed, Step::PanicElu]), Some(Refusal::BucketEmpty));
    assert_eq!(first(&[Step::Capacity, Step::Shed, Step::PanicElu, Step::Bucket]), None);
}

/// The router reads only its rungs' inputs, and stops at the first refusal.
#[test]
fn the_router_reads_its_rungs_up_to_the_first_refusal() {
    let readings = Readings::admitting(&[Step::Capacity]);
    let refused = first_refusal(Class::Normal, &readings);
    assert_eq!(refused.map(|r| r.reason), Some(Refusal::CapShed));
    assert_eq!(
        *readings.read.borrow(),
        [Step::Capacity, Step::Shed],
        "the bucket is never peeked past a refusal"
    );
}

/// Every refusal the ladder states belongs to exactly one rung; the backlog's
/// is the transaction layer's.
#[test]
fn each_ladder_reason_names_one_rung() {
    let rungs = [
        Rung::Brake { depth: 1, threshold: 0 },
        capacity(10, 0, None),
        Rung::Shed { at_threshold: true },
        Rung::PanicElu { elu: 1.0, threshold: 0.5 },
        Rung::Bucket { wait_sec: 1 },
    ];
    let mut reasons: Vec<Refusal> =
        rungs.iter().filter_map(|rung| reason(*rung, Class::Normal)).collect();
    reasons.push(Refusal::DeferredBacklog);
    let mut unique = reasons.clone();
    unique.sort_by_key(|r| r.index());
    unique.dedup();
    assert_eq!(reasons.len(), LADDER.len());
    assert_eq!(unique.len(), reasons.len());
}

fn invite(to_tag: bool, priority: Option<&str>) -> SipRequest {
    let to_tag = if to_tag { ";tag=b" } else { "" };
    let priority = priority.map(|p| format!("Resource-Priority: {p}\r\n")).unwrap_or_default();
    let raw = format!(
        "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-c\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=a\r\n\
To: <sip:bob@b2bua.test>{to_tag}\r\n\
Call-ID: class@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
{priority}Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// The class: in a dialog by its To-tag first, else emergency by an RFC 4412
/// emergency priority, else normal.
#[test]
fn the_class_of_an_invite() {
    assert_eq!(class_of(&invite(false, None)), Class::Normal);
    assert_eq!(class_of(&invite(false, Some("esnet.0"))), Class::Emergency);
    assert_eq!(class_of(&invite(false, Some("dsn.flash"))), Class::Normal);
    assert_eq!(class_of(&invite(true, Some("esnet.0"))), Class::InDialog);
    assert_eq!(class_of(&invite(true, None)), Class::InDialog);
    assert_eq!(CLASSES, Class::ALL);
}

/// The backlog ceilings the transaction layer is handed classify with the
/// ladder's one classifier.
#[test]
fn the_backlog_bound_classifies_with_the_ladder() {
    let bound = deferred_bound(4);
    for (req, class) in [
        (invite(false, None), Class::Normal),
        (invite(false, Some("esnet.0")), Class::Emergency),
        (invite(true, None), Class::InDialog),
    ] {
        assert_eq!((bound.class_of)(&req), class);
    }
}

/// The capacity rung reads the gate's ceilings and its last RSS sample.
#[test]
fn the_capacity_reading_is_the_gates() {
    let (probe, control) = simulated();
    let gate = CapacityGate::new(Arc::new(probe));
    let limits = CapacityConfig {
        rss_bytes: Ceilings { normal: Some(10), emergency: None },
        ..Default::default()
    };
    gate.configure(&limits);
    control.set_rss_bytes(Some(10));
    let occupancy = Occupancy { calls: 1, transactions: 2 };
    assert_eq!(gate.reading(occupancy).rss, None, "no sample taken yet");
    gate.sample(occupancy);
    let reading = gate.reading(occupancy);
    assert_eq!(reading, CapacityReading { limits, occupancy, rss: Some(10) });
    assert_eq!(reason(Rung::Capacity(reading), Class::Normal), Some(Refusal::CapacityRss));
}
