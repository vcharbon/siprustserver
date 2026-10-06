//! ADR-0040 decision 8 — a call materialised on another node (a takeover
//! copy, a reclaim) numbers its next admit above every number the previous
//! holder could have reserved without replicating it, the equal case
//! included: the materialisation moves the call's change counter to its next
//! epoch.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::{CallState, InMemoryCallStore, MaterialiseOrigin};
use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::SipRequest;
use sip_message::SipStr;

/// The URI a fixture names as text.
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
            Via::udp("127.0.0.1", 5060).with_branch(SipStr::owned(&format!("z9hG4bK{call_id}"))),
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
    CallState::new(store, "w0", B2buaMetrics::new()).with_clock(clock)
}

fn call(call_id: &str, created_at: i64) -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(
        &invite(call_id),
        src,
        &B2buaConfig::default(),
        &sip_txn::IdGen::seeded(1),
        created_at,
    )
}

/// The previous holder reserved numbers up to 4 and its write replicated 4
/// (or less): a copy materialised from that write numbers its next admit
/// above every number the holder could still send, 4 included.
#[tokio::test(start_paused = true)]
async fn a_materialised_call_numbers_its_next_admit_above_the_previous_holder_s() {
    for origin in [MaterialiseOrigin::Takeover, MaterialiseOrigin::Reclaim] {
        let s = state(Clock::test_at(0));
        let mut copy = call("epoch@x", 0);
        let replicated = copy.limiter.owe_consult(4) + 3;
        assert!(s.materialize_if_absent(copy.clone(), origin));
        let mut resident = s.peek(&copy.call_ref).expect("resident");
        let (next, _) = resident.limiter.number_admit();
        assert!(next > replicated, "{origin:?}: above the replicated number");
        assert!(
            next > call::CHANGE_EPOCH - 1,
            "{origin:?}: above every number the previous holder's epoch holds, got {next}"
        );
    }
}
