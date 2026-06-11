//! ADR-0020 X4 — the last-touched ledger on `CallState`: stamped at every
//! materialisation site, monotonic, membership mirroring the call map,
//! takeover copies excluded from the sweep view, and the idle clock restarting
//! at reclaim (never `created_at` — the stale-KeepaliveTimeout bug class).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::{BufferedTerminateWriter, CallState, InMemoryCallStore};
use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, ContactSpec, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
    SipTransport, ViaSpec,
};
use sip_message::SipRequest;

fn invite(call_id: &str) -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: "sip:bob@127.0.0.1:5070".into(),
        call_id: call_id.into(),
        from_uri: "sip:alice@host".into(),
        from_tag: "atag".into(),
        to_uri: "sip:bob@host".into(),
        to_tag: None,
        cseq: 1,
        via: Some(ViaSpec {
            local_ip: "127.0.0.1".into(),
            local_port: 5060,
            transport: SipTransport::Udp,
            branch: format!("z9hG4bK{call_id}"),
            custom_params: vec![],
        }),
        contact: Some(ContactSpec {
            user: "alice".into(),
            host: "127.0.0.1".into(),
            port: 5060,
            uri_params: vec![],
        }),
        max_forwards: Some(70),
        body: vec![],
        content_type: None,
        extra_headers: vec![],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts)
}

fn state(clock: Clock) -> CallState {
    let store = Arc::new(InMemoryCallStore::new());
    let writer = BufferedTerminateWriter::spawn(store.clone(), 64);
    CallState::new(store, writer, "w0", B2buaMetrics::new()).with_clock(clock)
}

fn call(call_id: &str, created_at: i64) -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(&invite(call_id), src, &B2buaConfig::default(), created_at)
}

#[tokio::test(start_paused = true)]
async fn ledger_is_monotonic_and_mirrors_the_call_map() {
    let clock = Clock::test_at(0);
    let s = state(clock.clone());

    let r = s.create(call("c1@x", clock.now_ms()));
    let stamped = s.last_touched(&r).expect("create stamps");

    // Monotonic-max: an older touch is ignored, a newer one advances.
    s.touch(&r, stamped - 1_000);
    assert_eq!(s.last_touched(&r), Some(stamped), "older touch ignored");
    s.touch(&r, stamped + 5_000);
    assert_eq!(s.last_touched(&r), Some(stamped + 5_000));

    // Membership mirrors the call map: a non-resident ref never enters.
    s.touch("w0|ghost|tag", 99);
    assert_eq!(s.last_touched("w0|ghost|tag"), None);
    assert_eq!(s.touched_count(), 1);

    // Eviction clears the stamp.
    s.remove(&r);
    assert_eq!(s.last_touched(&r), None);
    assert_eq!(s.touched_count(), 0, "no stamp survives removal");
}

#[tokio::test(start_paused = true)]
async fn stale_candidates_exclude_fresh_touched_and_takeover() {
    let clock = Clock::test_at(0);
    let s = state(clock.clone());

    let stale = s.create(call("stale@x", 0));
    let fresh = s.create(call("fresh@x", 0));
    let taken = s.create(call("taken@x", 0));

    tokio::time::advance(Duration::from_secs(100)).await;
    let now = clock.now_ms();

    // `fresh` got an event recently; `taken` is an acting-backup copy.
    s.touch(&fresh, now);
    s.mark_takeover(&taken);

    let refs: Vec<String> = s
        .stale_candidates(now, 50_000)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(refs, vec![stale.clone()], "only the untouched primary-served call is stale");

    // The watermark pairs the observed stamp.
    let (_, watermark) = s.stale_candidates(now, 50_000).pop().unwrap();
    assert_eq!(Some(watermark), s.last_touched(&stale));

    // drop_local (self-release) clears the stamp too.
    assert!(s.drop_local(&stale));
    assert_eq!(s.last_touched(&stale), None);
}

#[tokio::test(start_paused = true)]
async fn reclaim_restarts_the_idle_clock() {
    let clock = Clock::test_at(0);
    let s = state(clock.clone());

    // A 2 h-old long-hold call arrives via reclaim (materialize_if_absent).
    tokio::time::advance(Duration::from_secs(7_200)).await;
    let old_call = call("longhold@x", 0); // created_at = epoch 0, hours ago
    assert!(s.materialize_if_absent(old_call.clone()));

    // The idle clock starts at RECLAIM time, never created_at (ADR-0020 X4):
    // the freshly reclaimed call is NOT reap-stale.
    let now = clock.now_ms();
    assert!(
        s.stale_candidates(now, 90_000).is_empty(),
        "a freshly reclaimed hours-old call must not look stale"
    );
    assert_eq!(s.last_touched(&old_call.call_ref), Some(now));
}
