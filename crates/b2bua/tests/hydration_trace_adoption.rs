//! ADR-0026 §5 — trace adoption at the store's hydration seam.
//!
//! A node that materialises a replicated call opens THIS node's own root span
//! for it, and does so ONLY for a call it goes on to serve: an already-resident
//! call is left alone, so the registry never holds a span whose id no stored
//! call carries (a later takeover would link to a span no process ever served).
//!
//! The trace registry is process-wide, so this file holds exactly ONE test and
//! installs its own gate.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::{BufferedTerminateWriter, CallState, InMemoryCallStore, MaterialiseOrigin};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use observe::{RateDraw, SampleAdmission, TokenBucket};
use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::{SipRequest, SipStr};

const NOMINAL_TRACE: &str = "0123456789abcdef0123456789abcdef";
const NOMINAL_ROOT: &str = "fedcba9876543210";

fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn invite(call_id: &str) -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
        call_id: call_id.into(),
        from: Some(
            header::From::from_uri(uri_of("sip:alice@host")).with_tag(SipStr::from_static("atag")),
        ),
        to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
        cseq: 1,
        via: Some(
            Via::udp("127.0.0.1", 5060)
                .with_branch(SipStr::owned(&format!("z9hG4bK{call_id}"))),
        ),
        contact: Some(header::Contact::from_uri(
            Uri::sip_user("alice", "127.0.0.1").with_port(5060),
        )),
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

/// A replica body as it arrives from another node: `sampled` and carrying the
/// nominal's correlation triple.
fn replicated(call_id: &str, sampled: Option<bool>) -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    let mut c = build_initial_call(&invite(call_id), src, &B2buaConfig::default(), 0);
    c.trace_id = Some(NOMINAL_TRACE.to_string());
    c.root_span_id = Some(NOMINAL_ROOT.to_string());
    c.sampled = sampled;
    c
}

fn sample_everything() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(3), TokenBucket::default_at(0)),
        false,
    )));
}

#[tokio::test(start_paused = true)]
async fn materialising_adopts_the_call_it_serves_and_only_that_call() {
    sample_everything();
    let s = state(Clock::test_at(0));

    // ── Materialised here → this node's own root span, stamped on the copy the
    //    store keeps, and the registry agrees with it ──────────────────────────
    let served = replicated("served@x", Some(true));
    let served_ref = served.call_ref.clone();
    assert!(
        s.materialize_if_absent(served, MaterialiseOrigin::Reclaim),
        "a call absent here is materialised"
    );

    let stored = s.peek(&served_ref).expect("the materialised call is resident");
    assert_eq!(stored.trace_id.as_deref(), Some(NOMINAL_TRACE), "one trace across the takeover");
    let own_root = stored.root_span_id.clone().expect("this node stamped its own root span");
    assert_ne!(own_root, NOMINAL_ROOT, "the node serves under its OWN root span");
    assert_eq!(traces().active(), 1, "exactly one root span is open");
    let registered = traces()
        .activate(&served_ref, b2bua::trace::registry::call_identity(&stored), None, 0)
        .expect("the open span answers for the call");
    assert_eq!(
        registered.root_span_id, own_root,
        "the registered span IS the one the stored call names",
    );

    // ── Already resident → nothing is opened for a call this node does not take
    //    over here; the registry never outruns the stored state ────────────────
    let resident = replicated("resident@x", None);
    let resident_ref = resident.call_ref.clone();
    assert!(s.materialize_if_absent(resident, MaterialiseOrigin::Reclaim), "first materialise inserts");
    assert_eq!(traces().active(), 1, "an unsampled call opens no span");

    let again = replicated("resident@x", Some(true));
    assert!(
        !s.materialize_if_absent(again, MaterialiseOrigin::Reclaim),
        "a resident call is left untouched"
    );
    assert_eq!(
        traces().active(),
        1,
        "the discarded copy opened no span — a span with no call naming it would \
         make a later takeover link to a root no process ever served",
    );
    assert_eq!(
        s.peek(&resident_ref).and_then(|c| c.root_span_id),
        Some(NOMINAL_ROOT.to_string()),
        "the resident copy is unchanged",
    );

    traces().close(&served_ref);
    assert_eq!(traces().active(), 0, "the call's span closes with it");
}
