//! Shared test fixtures: a representative [`Call`] (port of
//! `tests/bench/call-codec/fixture.ts`) and a proptest [`Strategy`] that
//! generates varied call trees (the Rust analogue of the source fixture mixer).
//!
//! Each integration-test binary `mod common`s this file, so not every binary
//! uses every helper — silence the cross-binary dead-code noise here.
#![allow(dead_code)]

use std::collections::BTreeMap;

use call::features::{
    FeatureActivations, KeepaliveActivation, PlatformActivations, RelayFirst18xStrategy,
    RelayFirst18xTo180Feature,
};
use call::model::*;
use proptest::prelude::*;
use sip_retransmit::Class;

mod emission;
mod provisional;
mod request_record;
mod sdp_session;
use emission::{arb_own_notify, arb_retained_emission};
use provisional::{arb_owed_answer, arb_provisional};
use request_record::{arb_invite_record, arb_pending_request};
use sdp_session::arb_sdp_session;

// ── Representative fixture (port of fixture.ts) ─────────────────────────────

const SDP_BODY: &[u8] = b"v=0\r\no=alice 53655765 2353687637 IN IP4 192.0.2.1\r\n\
s=B2BUA call\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 16384 RTP/AVP 0 8 96\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\na=ptime:20\r\n";

fn header(name: &str, value: &str) -> SipHeader {
    SipHeader { name: name.to_string(), value: value.to_string() }
}

/// A confirmed dialog of a fixture leg: its tags, remote target, own CSeq
/// (the peer's one above), the cached answer, `pending` relayed INFOs and,
/// with any, a relayed re-INVITE awaiting its final.
fn dialog(
    local_tag: &str,
    remote_tag: &str,
    remote_target: &str,
    cseq: i64,
    cached_sdp: bool,
    pending: usize,
    source_call_id: &str,
) -> Dialog {
    let mut d = Dialog::new(local_tag, remote_tag, DialogState::Confirmed, cseq);
    d.peer_cseq = Some(cseq + 1);
    d.remote_target = remote_target.to_string();
    d.route_set = vec!["<sip:fproxy.example.com;lr>".into(), "<sip:edge1.example.com;lr>".into()];
    d.requests = (0..pending)
        .map(|i| RequestRecord {
            source_vias: vec!["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-x".into()],
            source_call_id: source_call_id.to_string(),
            source_from: "<sip:alice@example.com>;tag=alice-001".into(),
            source_to: "<sip:bob@example.com>;tag=b2bua-aleg".into(),
            ..RequestRecord::relayed("INFO", cseq + 10 + i as i64, cseq + 10 + i as i64)
        })
        .collect();
    d.sent_invite = (pending > 0).then(|| RequestRecord {
        source_vias: vec!["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-y".into()],
        source_call_id: source_call_id.to_string(),
        source_from: "<sip:alice@example.com>;tag=alice-001".into(),
        source_to: "<sip:bob@example.com>;tag=b2bua-aleg".into(),
        ..RequestRecord::relayed("INVITE", cseq + 9, cseq + 9)
    });
    d.cached_sdp = cached_sdp.then(|| SDP_BODY.to_vec());
    d
}

/// What a fixture emission on the ladder `class` paces is a copy of: an
/// INVITE's 200 on the §13.3.1.4 ladder, a 183 to an INVITE on RFC 3262 §3's.
pub fn repeated_on(class: Class) -> Repeated {
    match class {
        Class::ReliableProvisional => Repeated::response("INVITE", 183),
        _ => Repeated::response("INVITE", 200),
    }
}

/// A relayed INVITE numbered `cseq` whose 2xx this side took, its ACK awaited
/// from the acknowledging peer under that number (RFC 3261 §13.2.2.4).
pub fn invite_awaiting_ack(cseq: i64) -> RequestRecord {
    let mut taken = RequestRecord::relayed("INVITE", cseq, cseq);
    if let Some(x) = taken.invite.as_mut() {
        x.awaited_ack_cseq = Some(cseq);
    }
    taken
}

/// A retained emission standing on `rung` of the ladder `class` paces: the
/// first rung armed by `paced`, then advanced rung by rung — the way the
/// stack itself reaches one.
pub fn paced_emission(
    datagram: &[u8],
    dest: (&str, u16),
    class: Class,
    rung: u32,
) -> RetainedEmission {
    let (mut emission, _) = RetainedEmission::paced(
        datagram.to_vec(),
        (dest.0.to_string(), dest.1),
        class,
        repeated_on(class),
    );
    for _ in 1..rung {
        emission.advance(None).expect("the requested rung sits inside the class's bound");
    }
    emission
}

/// The initial-INVITE 2xx the representative call answered the caller with,
/// three rungs into its §13.3.1.4 ladder (RFC 3261) — the caller's ACK is
/// still outstanding.
pub const ANSWERED_2XX: &[u8] = b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-123;rport=5060\r\n\
From: <sip:alice@example.com>;tag=alice-from-tag-001\r\nTo: <sip:bob@example.com>;tag=b2bua-to-tag-aleg-9876\r\n\
Call-ID: call-id-deadbeef@example.com\r\nCSeq: 1 INVITE\r\nP-Charging-Vector: icid-value=icid-9f2c\r\n\
Content-Length: 0\r\n\r\n";

/// The ACK the representative call emitted toward the callee, retained for
/// the §13.2.2.4 re-ACK.
pub const EMITTED_ACK: &[u8] = b"ACK sip:bob@192.0.2.20:5060 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-ack-1f45\r\n\
CSeq: 8005 ACK\r\nContent-Length: 0\r\n\r\n";

/// A realistic confirmed 2-leg call — mirrors `representativeCall` in fixture.ts.
pub fn representative_call() -> Call {
    let mut a_leg = Leg {
        leg_id: "a".into(),
        call_id: "call-id-deadbeef@example.com".into(),
        source: RemoteInfo { address: "10.0.0.5".into(), port: 5060 },
        dialogs: vec![dialog(
            "b2bua-to-tag-aleg-9876",
            "alice-from-tag-001",
            "sip:alice@192.0.2.10:5060;transport=udp",
            8005,
            false,
            2,
            "call-id-deadbeef@example.com",
        )]
        .into(),
        local_uri: Some("sip:bob@example.com".into()),
        remote_uri: Some("sip:alice@example.com".into()),
        invite_request_uri: None,
        invite: None,
        own_tag: Some("b2bua-to-tag-aleg-9876".into()),
        ext: None,
        kind: None,
        adopted: None,
        attempt: None,
        ring: Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: LegSdpSession {
            sent_origin: Some("bob 202 3 IN IP4 203.0.113.42".into()),
            sent_media: vec!["audio 20000 RTP/AVP 8".into(), "video 0 RTP/AVP 96".into()],
            sent_digest: Some(0x5eed_d16e_5700_0001),
            absorbed_origin: Some("bob 202 2 IN IP4 203.0.113.42".into()),
            sent_slots: vec![Some(0), None],
            sent_slots_author: Some("b-3".into()),
            restated_from: Some("1 carol 303 1 IN IP4 192.0.2.3".into()),
            exchanges_opened: 1,
            session_author: Some("b-2".into()),
            session_id: Some("202".into()),
            restated: true,
            has_restated: true,
        },
    };
    a_leg.dialogs.confirmed_mut().expect("the answered dialog").fronts =
        vec![PeerDialog { leg_id: "b-1".into(), peer_tag: "bob-to-tag-007".into() }];
    *a_leg.invite_final_sent_mut() = Some(200);
    call::helpers::hold_answered_2xx(
        a_leg.dialogs.confirmed_mut().expect("the answered dialog"),
        1,
        Unacked2xx {
            dialog_tag: "b2bua-to-tag-aleg-9876".into(),
            cseq: 1,
            emission: paced_emission(ANSWERED_2XX, ("192.0.2.10", 5060), Class::Final2xx, 3),
        },
    );
    let mut b_leg = Leg {
        leg_id: "b-1".into(),
        call_id: "b-leg-call-id-fedcba@b2bua".into(),
        source: RemoteInfo { address: "203.0.113.42".into(), port: 5060 },
        dialogs: vec![dialog(
            "b2bua-from-tag-bleg-5544",
            "bob-to-tag-007",
            "sip:bob@203.0.113.42:5060;transport=udp",
            4002,
            true,
            1,
            "b-leg-call-id-fedcba@b2bua",
        )]
        .into(),
        local_uri: Some("sip:alice@example.com".into()),
        remote_uri: Some("sip:bob@example.com".into()),
        invite_request_uri: Some("sip:bob@example.com".into()),
        invite: Some(RequestRecord::dialled(
            1,
            None,
            OutboundInvite::new("b2bua-from-tag-bleg-5544", 0),
        )),
        own_tag: None,
        ext: None,
        kind: None,
        adopted: None,
        attempt: None,
        ring: Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: LegSdpSession::default(),
    };
    let b_dialog = b_leg.dialogs.confirmed_mut().expect("the confirmed dialog");
    let mut b_invite =
        b_dialog.sent_invite.take().unwrap_or_else(|| RequestRecord::relayed("INVITE", 8005, 0));
    if let Some(x) = b_invite.invite.as_mut() {
        x.ack_branch = Some("z9hG4bK-ack-1f45".into());
        x.emitted_ack = Some(RetainedEmission::on_trigger(
            EMITTED_ACK.to_vec(),
            ("192.0.2.20".into(), 5060),
            Repeated::request("ACK"),
        ));
    }
    b_dialog.sent_invite = Some(b_invite);

    let mut ext: ExtMap = BTreeMap::new();
    ext.insert(
        "promote-pem".into(),
        serde_json::json!({ "promoted": true, "windowOpen": false, "resyncReinviteCSeq": 8007 }),
    );

    Call {
        call_ref: "worker-0|call-id-deadbeef@example.com|alice-from-tag-001".into(),
        a_leg,
        b_legs: vec![b_leg],
        attempts: Vec::new(),
        active_peer: Some(ActivePeer { leg_a: "a".into(), leg_b: "b-1".into() }),
        callback_context: Some("ctx-abc-123".into()),
        billing_context: Some("subscriber=alice@example.com;plan=premium".into()),
        a_leg_invite: ALegInviteSnapshot {
            uri: "sip:bob@example.com".into(),
            headers: vec![
                header("Via", "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-123;rport"),
                header("From", "<sip:alice@example.com>;tag=alice-from-tag-001"),
                header("To", "<sip:bob@example.com>"),
                header("Call-ID", "call-id-deadbeef@example.com"),
                header("CSeq", "1 INVITE"),
                header("Content-Type", "application/sdp"),
            ],
            body: SDP_BODY.to_vec(),
            cseq: 1,
            from_tag: "alice-from-tag-001".into(),
        },
        limiter: CallLimiterState::admitted(
            "worker-0|call-id-deadbeef@example.com|alice-from-tag-001#k1".into(),
            1,
            vec![LimiterEntry { id: "subscriber:alice@example.com".into(), limit: 20 }],
        ),
        timers: vec![
            TimerEntry {
                key: TimerKey::leg(TimerType::NoAnswer, "a"),
                generation: 1,
                fire_at: 1_779_440_045_000,
            },
            TimerEntry {
                key: TimerKey::call(TimerType::GlobalDuration),
                generation: 2,
                fire_at: 1_779_443_642_000,
            },
        ],
        timer_generation: 2,
        cdr_events: vec![
            CdrEvent {
                event_type: CdrEventType::InviteReceived,
                timestamp: 1_779_440_042_000,
                leg_id: "a".into(),
                status_code: None,
                reason: None,
                decision_ordinal: 0,
            },
            CdrEvent {
                event_type: CdrEventType::Answer,
                timestamp: 1_779_440_043_200,
                leg_id: "b-1".into(),
                status_code: Some(200),
                reason: None,
                decision_ordinal: 1,
            },
        ],
        state: CallModelState::Active,
        created_at: 1_779_440_042_000,
        trace_id: Some("0123456789abcdef0123456789abcdef".into()),
        root_span_id: Some("fedcba9876543210".into()),
        sampled: Some(true),
        worker_index: Some(0),
        topology: Some(CallTopology::new("worker-0", "worker-1", 3, 0)),
        emergency: Some(true),
        features: Some(FeatureActivations {
            platform: PlatformActivations {
                max_duration_ms: 3_600_000,
                max_duration_anchor: Default::default(),
                keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: Some(RelayFirst18xTo180Feature {
                strategy: RelayFirst18xStrategy::FakePrack,
                messages: Default::default(),
            }),
            no_answer_ms: Some(45_000),
            advertise_capabilities: None,
            charging_vector: None,
            withhold_option_tags: None,
            stated_headers: Some(representative_stated_headers()),
            uncharged_media_legs: true,
            withhold_on_relayed_provisionals: Some(vec!["P-Asserted-Identity".into()]),
        }),
        policy_update_headers: None,
        policy_update_body: None,
        active_rules: Some(vec![
            ActiveRule { id: "limit-by-subscriber".into(), active: true },
            ActiveRule { id: "promote-pem-to-200".into(), active: false },
        ]),
        ext: Some(ext),
        message_count: Some(7),
        terminating_refresh_legs: None,
        relay_first_18x: None,
        promote_pem: None,
        transfer: None,
        subscriptions: vec![ReleaseEventKind::MaxCallDuration],
        reroute: Some(RerouteState {
            phase: ReroutePhase::ARealigning,
            new_leg_id: "b-2".into(),
            old_leg_id: Some("b-1".into()),
            started_at_ms: 1_779_440_050_000,
        }),
        provisionals: Vec::new(),
        message_seq: 0,
        decision_log: vec![representative_mark()],
        decision_ordinal: 1,
        termination: None,
        message_turn: 1,
        sm_cursors: BTreeMap::new(),
    }
}

/// The one decision the representative call was routed under.
fn representative_mark() -> DecisionMark {
    DecisionMark {
        ordinal: 1,
        at_ms: 1_779_440_042_010,
        kind: DecisionKind::Route,
        leg_id: Some("a".into()),
        label: Some("plan-7".into()),
    }
}

// ── proptest strategies ─────────────────────────────────────────────────────

mod records;
use records::{
    arb_attempt, arb_cdr, arb_decision_log, arb_message_ring, arb_ring, arb_termination, arb_timer,
};

fn arb_tag() -> impl Strategy<Value = String> {
    "[a-z0-9-]{1,16}"
}
fn arb_uri() -> impl Strategy<Value = String> {
    "sip:[a-z0-9@.:-]{3,24}"
}
fn arb_bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..max)
}

fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        "[a-z0-9 ]{0,16}".prop_map(serde_json::Value::from),
        proptest::collection::vec(any::<i64>(), 0..4).prop_map(serde_json::Value::from),
    ]
}
fn arb_ext() -> impl Strategy<Value = Option<ExtMap>> {
    proptest::option::of(proptest::collection::btree_map("[a-z]{1,8}", arb_json(), 0..3))
}

/// Varied `sm_cursors` maps (ADR-0016): empty through a few
/// `MachineId → StateLabel` entries, exercising the round-trip.
fn arb_sm_cursors() -> impl Strategy<Value = BTreeMap<MachineId, StateLabel>> {
    proptest::collection::btree_map(
        "[a-z-]{1,12}".prop_map(|s| MachineId(s.into())),
        "[A-Za-z]{1,12}".prop_map(|s| StateLabel(s.into())),
        0..3,
    )
}

fn arb_remote_info() -> impl Strategy<Value = RemoteInfo> {
    ("[a-z0-9.]{1,15}", any::<u16>()).prop_map(|(address, port)| RemoteInfo { address, port })
}
fn arb_host_port() -> impl Strategy<Value = HostPort> {
    ("[a-z0-9.]{1,15}", any::<u16>()).prop_map(|(host, port)| HostPort { host, port })
}
fn arb_leg_kind() -> impl Strategy<Value = LegKind> {
    prop_oneof![
        Just(LegKind::A),
        Just(LegKind::Destination),
        Just(LegKind::Media),
        Just(LegKind::TransferTarget),
    ]
}
fn arb_timer_type() -> impl Strategy<Value = TimerType> {
    prop_oneof![
        Just(TimerType::NoAnswer),
        Just(TimerType::GlobalDuration),
        Just(TimerType::LimiterRefresh),
        Just(TimerType::Keepalive),
        Just(TimerType::KeepaliveTimeout),
        Just(TimerType::TerminatingTimeout),
        Just(TimerType::ReferSubscriptionExpiry),
        Just(TimerType::ReferReinviteAnswer),
        Just(TimerType::ReferOverallSafety),
        Just(TimerType::NotifyAnswerCap),
        Just(TimerType::HeldAnswer),
        // Service-owned timer: arbitrary (service_id, key) — both deserialise
        // into owned strings on the replica side (Cow::Owned), so the codec
        // P1 round-trip property covers the HA replication path for it.
        ("[a-z][a-z0-9-]{0,8}", "[a-zA-Z0-9_-]{1,12}").prop_map(|(sid, key)| {
            TimerType::Service { service_id: MachineId(sid.into()), key: key.into() }
        }),
    ]
}
fn arb_cdr_type() -> impl Strategy<Value = CdrEventType> {
    prop_oneof![
        Just(CdrEventType::InviteReceived),
        Just(CdrEventType::InviteSent),
        Just(CdrEventType::Provisional),
        Just(CdrEventType::Answer),
        Just(CdrEventType::Bye),
        Just(CdrEventType::Cancel),
        Just(CdrEventType::Timeout),
        Just(CdrEventType::Reject),
    ]
}
fn arb_direction() -> impl Strategy<Value = Direction> {
    prop_oneof![Just(Direction::FromA), Just(Direction::FromB)]
}

/// Boxed, as [`arb_leg`] is: a debug build otherwise holds the whole value tree
/// of a call on the test thread's stack.
fn arb_dialog() -> BoxedStrategy<Dialog> {
    let sip = (
        arb_tag(),
        arb_tag(),
        arb_dialog_state(),
        arb_uri(),
        any::<i64>(),
        proptest::collection::vec("[a-z0-9 /.:;=<>-]{1,40}", 0..3),
        proptest::collection::vec(
            ("(a|b-[0-9]{1,2})", arb_tag())
                .prop_map(|(leg_id, peer_tag)| PeerDialog { leg_id, peer_tag }),
            0..2,
        ),
    );
    let ext = (
        proptest::option::of(any::<i64>()),
        proptest::collection::vec(arb_pending_request(), 0..3),
        proptest::option::of(arb_invite_record()),
        proptest::option::of(arb_invite_record()),
        proptest::option::of(arb_bytes(80)),
        (
            proptest::option::of(arb_sdp_session()),
            proptest::option::of(arb_owed_answer()),
            proptest::option::of(arb_own_notify()),
        ),
        (any::<bool>(), proptest::option::of(arb_tag())),
    );
    (sip, ext)
        .prop_map(
            |(
                (our_tag, peer_tag, state, remote_target, our_cseq, route_set, fronts),
                (
                    peer_cseq,
                    requests,
                    sent_invite,
                    answered_invite,
                    cached_sdp,
                    (sdp_session, owed_answer, own_notify),
                    (shown, shows_media),
                ),
            )| Dialog {
                our_tag,
                peer_tag,
                state,
                our_cseq,
                peer_cseq,
                remote_target,
                route_set,
                fronts,
                shown,
                shows_media,
                requests,
                sent_invite,
                answered_invite,
                cached_sdp,
                sdp_session,
                owed_answer,
                own_notify,
            },
        )
        .boxed()
}

/// Any dialog state, a retired one with open requests.
fn arb_dialog_state() -> impl Strategy<Value = DialogState> {
    let end = prop_oneof![
        any::<u16>().prop_map(DialogEnd::Rejected),
        Just(DialogEnd::PeerBye),
        Just(DialogEnd::EarlyPeerBye),
        proptest::option::of(any::<u16>())
            .prop_map(|final_status| DialogEnd::OurBye { final_status }),
    ];
    let retired = proptest::collection::vec(arb_pending_request(), 0..2)
        .prop_map(|open_requests| RetiredDialog { open_requests });
    prop_oneof![
        Just(DialogState::Early),
        Just(DialogState::Confirmed),
        Just(DialogState::Ending(EndingBy::Bye)),
        Just(DialogState::Ending(EndingBy::Cancel)),
        end.prop_map(DialogState::Ended),
        retired.prop_map(DialogState::Retired),
    ]
}

/// Boxed: a value tree this size, held by value in every enclosing tuple, is
/// past the 2 MiB test-thread stack of a debug build.
fn arb_leg() -> BoxedStrategy<Leg> {
    (
        "(a|b-[0-9]{1,2})",
        arb_tag(),
        (proptest::option::of(arb_invite_record()), proptest::option::of(arb_tag())),
        arb_remote_info(),
        proptest::collection::vec(arb_dialog(), 0..3),
        (
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
        ),
        (
            arb_ext(),
            proptest::option::of(arb_leg_kind()),
            proptest::option::of(any::<bool>()),
            proptest::option::of(any::<bool>()),
        ),
        arb_message_ring(),
        arb_sdp_session(),
        (proptest::option::of((0usize..4).prop_map(AttemptId)), arb_ring()),
    )
        .prop_map(
            |(
                leg_id,
                call_id,
                (invite, own_tag),
                source,
                dialogs,
                (local_uri, remote_uri, invite_request_uri),
                (ext, kind, adopted, in_session_timer),
                messages,
                sdp_session,
                (attempt, ring),
            )| Leg {
                leg_id,
                call_id,
                source,
                dialogs: dialogs.into(),
                local_uri,
                remote_uri,
                invite_request_uri,
                invite,
                own_tag,
                ext,
                kind,
                adopted,
                in_session_timer,
                attempt,
                ring,
                messages,
                sdp_session,
            },
        )
        .boxed()
}

fn arb_limiter() -> impl Strategy<Value = CallLimiterState> {
    let entries = || {
        proptest::collection::vec(
            (arb_tag(), 0i64..100).prop_map(|(id, limit)| LimiterEntry { id, limit }),
            0..3,
        )
    };
    let changes = (any::<u32>(), any::<u32>(), any::<u32>());
    let runs_on = proptest::option::of(entries());
    (arb_tag(), any::<bool>(), entries(), entries(), changes, runs_on).prop_map(
        |(key, sent, held, target, (held_change, target_change, above), runs_on)| {
            let (held_change, target_change) = (u64::from(held_change), u64::from(target_change));
            let release_owed = sent || !held.is_empty();
            CallLimiterState::from_parts(
                key,
                release_owed,
                (held, held_change),
                (target, target_change),
                held_change.max(target_change) + u64::from(above),
                runs_on,
            )
        },
    )
}
fn arb_active_rule() -> impl Strategy<Value = ActiveRule> {
    (arb_tag(), any::<bool>()).prop_map(|(id, active)| ActiveRule { id, active })
}
fn arb_features() -> impl Strategy<Value = FeatureActivations> {
    let platform = (any::<i64>(), any::<i64>(), any::<i64>()).prop_map(
        |(max_duration_ms, interval_sec, max_missed)| PlatformActivations {
            max_duration_ms,
            max_duration_anchor: Default::default(),
            keepalive: KeepaliveActivation { interval_sec, max_missed },
        },
    );
    let strat = prop_oneof![
        Just(RelayFirst18xStrategy::DropSdp),
        Just(RelayFirst18xStrategy::KeepSdp),
        Just(RelayFirst18xStrategy::FakePrack),
        Just(RelayFirst18xStrategy::PromotePemTo200),
    ];
    let messages = prop_oneof![
        Just(call::features::Relay18xMessages::All),
        Just(call::features::Relay18xMessages::First),
        Just(call::features::Relay18xMessages::OnePerValue),
    ];
    (
        platform,
        proptest::option::of(proptest::option::of(any::<i64>())),
        proptest::option::of((strat, messages)),
        proptest::option::of(any::<i64>()),
        proptest::option::of(arb_stated_headers()),
        proptest::option::of(proptest::collection::vec("[A-Za-z][A-Za-z-]{0,15}", 0..3)),
    )
        .prop_map(|(platform, refer_depth, strategy, no_answer, stated, withheld)| {
            FeatureActivations {
                platform,
                refer: refer_depth
                    .map(|max_chain_depth| call::features::ReferFeature { max_chain_depth }),
                relay_first_18x_to_180: strategy
                    .map(|(strategy, messages)| RelayFirst18xTo180Feature { strategy, messages }),
                no_answer_ms: no_answer,
                advertise_capabilities: None,
                charging_vector: None,
                withhold_option_tags: None,
                stated_headers: stated,
                uncharged_media_legs: false,
                withhold_on_relayed_provisionals: withheld,
            }
        })
}

/// One statement of each kind in each scope.
pub fn representative_stated_headers() -> call::features::StatedHeaders {
    use call::header_update::HeaderUpdate;
    let mut s = call::features::StatedHeaders::default();
    s.launched_invite.insert("X-Launch".into(), HeaderUpdate::Add(vec!["l".into()]));
    s.originator_finals.insert("X-Final".into(), HeaderUpdate::Set(vec!["f1".into(), "f2".into()]));
    s.every_message.insert("X-Gone".into(), HeaderUpdate::Remove);
    s.every_message.insert("X-Every".into(), HeaderUpdate::line("e"));
    let mut leg = call::features::StatedHeaders::default();
    leg.every_message.insert("X-Leg".into(), HeaderUpdate::line("b"));
    s.legs.insert("b-2".into(), leg);
    let mut earlier = call::features::StatedHeaders::default();
    earlier.every_message.insert("X-Every".into(), HeaderUpdate::line("earlier"));
    s.reverts_to = Some(Box::new(earlier));
    s
}

fn arb_header_update() -> impl Strategy<Value = call::header_update::HeaderUpdate> {
    use call::header_update::HeaderUpdate;
    let lines = proptest::collection::vec("[ -~]{0,20}", 0..3);
    prop_oneof![
        Just(HeaderUpdate::Remove),
        lines.clone().prop_map(HeaderUpdate::Set),
        lines.prop_map(HeaderUpdate::Add),
    ]
}

fn arb_header_updates() -> impl Strategy<Value = call::header_update::SipHeaderUpdates> {
    proptest::collection::btree_map("[A-Za-z-]{1,16}", arb_header_update(), 0..4)
}

fn arb_scoped_sets() -> impl Strategy<Value = call::features::StatedHeaders> {
    (arb_header_updates(), arb_header_updates(), arb_header_updates()).prop_map(
        |(launched_invite, originator_finals, every_message)| call::features::StatedHeaders {
            launched_invite,
            originator_finals,
            every_message,
            legs: Default::default(),
            reverts_to: None,
        },
    )
}

fn arb_stated_headers() -> impl Strategy<Value = call::features::StatedHeaders> {
    (
        arb_scoped_sets(),
        proptest::collection::btree_map("b-[1-3]", arb_scoped_sets(), 0..2),
        proptest::option::of(arb_scoped_sets()),
    )
        .prop_map(|(mut sets, legs, reverts_to)| {
            sets.legs = legs;
            sets.reverts_to = reverts_to.map(Box::new);
            sets
        })
}

fn arb_aleg_invite() -> impl Strategy<Value = ALegInviteSnapshot> {
    (
        arb_uri(),
        proptest::collection::vec(
            ("[A-Za-z-]{1,16}", "[ -~]{0,40}").prop_map(|(name, value)| SipHeader { name, value }),
            0..6,
        ),
        // Cover P7 binary integrity across small + large bodies (incl. empty).
        prop_oneof![arb_bytes(4), arb_bytes(2048)],
        any::<u32>(),
        arb_tag(),
    )
        .prop_map(|(uri, headers, body, cseq, from_tag)| ALegInviteSnapshot {
            uri,
            headers,
            body,
            cseq,
            from_tag,
        })
}

fn arb_policy_body() -> impl Strategy<Value = Option<PolicyUpdateBody>> {
    prop_oneof![
        Just(None),
        Just(Some(PolicyUpdateBody::Empty)),
        arb_bytes(64).prop_map(|b| Some(PolicyUpdateBody::Bytes(b))),
    ]
}
fn arb_policy_headers() -> impl Strategy<Value = Option<BTreeMap<String, Option<String>>>> {
    proptest::option::of(proptest::collection::btree_map(
        "[A-Za-z-]{1,12}",
        proptest::option::of("[ -~]{0,20}"),
        0..3,
    ))
}

/// The Rust analogue of the fixture mixer: a richly-varied [`Call`] tree.
pub fn arb_call() -> impl Strategy<Value = Call> {
    let head = (
        "[a-z0-9|@.-]{1,32}",
        arb_leg(),
        proptest::collection::vec(arb_leg(), 0..3),
        proptest::option::of(
            (arb_tag(), arb_tag()).prop_map(|(leg_a, leg_b)| ActivePeer { leg_a, leg_b }),
        ),
        proptest::option::of("[a-z0-9-]{0,20}"),
        proptest::option::of("[a-z0-9=@.;-]{0,30}"),
        arb_aleg_invite(),
    );
    let collections = (
        arb_limiter(),
        proptest::collection::vec(arb_timer(), 0..4),
        proptest::collection::vec(arb_cdr(), 0..4),
        proptest::collection::vec(arb_attempt(), 0..3),
    );
    let state = (
        prop_oneof![
            Just(CallModelState::Active),
            Just(CallModelState::Terminating),
            Just(CallModelState::Terminated),
        ],
        any::<i64>(),
    );
    let trace = (
        proptest::option::of("[0-9a-f]{32}"),
        proptest::option::of("[0-9a-f]{16}"),
        proptest::option::of(any::<bool>()),
        proptest::option::of(any::<i64>()),
        proptest::option::of(
            (arb_tag(), arb_tag(), any::<i64>(), any::<i64>())
                .prop_map(|(pri, bak, gen, bak_gen)| CallTopology::new(pri, bak, gen, bak_gen)),
        ),
    );
    let tail = (
        proptest::option::of(any::<bool>()),
        proptest::option::of(arb_features()),
        arb_policy_headers(),
        arb_policy_body(),
        proptest::option::of(proptest::collection::vec(arb_active_rule(), 0..3)),
        arb_ext(),
        proptest::option::of(any::<i64>()),
        proptest::option::of(proptest::collection::vec(arb_tag(), 0..3)),
        arb_sm_cursors(),
        any::<u32>(),
    );
    // Release-event subscriptions + the in-flight reroute slice
    // ride the replicated body like `features`/`transfer` do.
    let release = (
        proptest::collection::vec(arb_provisional(), 0..3),
        arb_decision_log(),
        arb_termination(),
        proptest::collection::vec(Just(ReleaseEventKind::MaxCallDuration), 0..2),
        proptest::option::of(
            (arb_tag(), proptest::option::of(arb_tag()), any::<i64>(), any::<bool>()).prop_map(
                |(new_leg_id, old_leg_id, started_at_ms, realigning)| RerouteState {
                    phase: if realigning {
                        ReroutePhase::ARealigning
                    } else {
                        ReroutePhase::BLegDialing
                    },
                    new_leg_id,
                    old_leg_id,
                    started_at_ms,
                },
            ),
        ),
        any::<u32>(),
    );

    (head, collections, state, trace, tail, release).prop_map(
        |(
            (call_ref, a_leg, b_legs, active_peer, callback_context, billing_context, a_leg_invite),
            (limiter, timers, cdr_events, attempts),
            (state, created_at),
            (trace_id, root_span_id, sampled, worker_index, topology),
            (
                emergency,
                features,
                policy_update_headers,
                policy_update_body,
                active_rules,
                ext,
                message_count,
                terminating_refresh_legs,
                sm_cursors,
                message_seq,
            ),
            (provisionals, decision_log, termination, subscriptions, reroute, message_turn),
        )| Call {
            call_ref,
            a_leg,
            b_legs,
            attempts,
            active_peer,
            callback_context,
            billing_context,
            a_leg_invite,
            limiter,
            timers,
            timer_generation: 0,
            cdr_events,
            state,
            created_at,
            trace_id,
            root_span_id,
            sampled,
            worker_index,
            topology,
            emergency,
            features,
            policy_update_headers,
            policy_update_body,
            active_rules,
            ext,
            message_count,
            terminating_refresh_legs,
            relay_first_18x: None,
            promote_pem: None,
            transfer: None,
            subscriptions,
            reroute,
            provisionals,
            message_seq,
            decision_ordinal: decision_log.len() as u32,
            decision_log,
            termination,
            message_turn,
            sm_cursors,
        },
    )
}
