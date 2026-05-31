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

// ── Representative fixture (port of fixture.ts) ─────────────────────────────

const SDP_BODY: &[u8] = b"v=0\r\no=alice 53655765 2353687637 IN IP4 192.0.2.1\r\n\
s=B2BUA call\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 16384 RTP/AVP 0 8 96\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\na=ptime:20\r\n";

fn header(name: &str, value: &str) -> SipHeader {
    SipHeader {
        name: name.to_string(),
        value: value.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn dialog(
    call_id: &str,
    local_tag: &str,
    remote_tag: &str,
    local_uri: &str,
    remote_uri: &str,
    remote_target: &str,
    cseq: i64,
    cached_sdp: bool,
    pending: usize,
) -> Dialog {
    Dialog {
        sip: StackDialog {
            call_id: call_id.to_string(),
            local_tag: local_tag.to_string(),
            remote_tag: remote_tag.to_string(),
            local_uri: local_uri.to_string(),
            remote_uri: remote_uri.to_string(),
            remote_target: remote_target.to_string(),
            local_cseq: cseq,
            route_set: vec![
                "<sip:fproxy.example.com;lr>".into(),
                "<sip:edge1.example.com;lr>".into(),
            ],
        },
        ext: B2buaDialogExt {
            remote_cseq: Some(cseq + 1),
            inbound_pending_requests: (0..pending)
                .map(|i| PendingRequest {
                    method: "INVITE".into(),
                    outbound_cseq: cseq + 10 + i as i64,
                    inbound_cseq: cseq + 10 + i as i64,
                    source_vias: vec!["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-x".into()],
                    source_call_id: call_id.to_string(),
                    source_from: "<sip:alice@example.com>;tag=alice-001".into(),
                    source_to: "<sip:bob@example.com>;tag=b2bua-aleg".into(),
                    direction: Direction::FromA,
                })
                .collect(),
            ack_branch: Some(format!("z9hG4bK-ack-{cseq:x}")),
            pending_invite_txn: None,
            cached_sdp: cached_sdp.then(|| SDP_BODY.to_vec()),
        },
    }
}

/// A realistic confirmed 2-leg call — mirrors `representativeCall` in fixture.ts.
pub fn representative_call() -> Call {
    let a_leg = Leg {
        leg_id: "a".into(),
        call_id: "call-id-deadbeef@example.com".into(),
        from_tag: "alice-from-tag-001".into(),
        source: RemoteInfo {
            address: "10.0.0.5".into(),
            port: 5060,
        },
        state: LegState::Confirmed,
        disposition: LegDisposition::Bridged,
        dialogs: vec![dialog(
            "call-id-deadbeef@example.com",
            "b2bua-to-tag-aleg-9876",
            "alice-from-tag-001",
            "sip:bob@example.com",
            "sip:alice@example.com",
            "sip:alice@192.0.2.10:5060;transport=udp",
            8005,
            false,
            2,
        )],
        no_answer_timeout_sec: None,
        bye_disposition: None,
        local_uri: Some("sip:bob@example.com".into()),
        remote_uri: Some("sip:alice@example.com".into()),
        invite_request_uri: None,
        pending_invite_txn: None,
        ext: None,
        kind: None,
        adopted: None,
    };
    let b_leg = Leg {
        leg_id: "b-1".into(),
        call_id: "b-leg-call-id-fedcba@b2bua".into(),
        from_tag: "b2bua-from-tag-bleg-5544".into(),
        source: RemoteInfo {
            address: "203.0.113.42".into(),
            port: 5060,
        },
        state: LegState::Confirmed,
        disposition: LegDisposition::Bridged,
        dialogs: vec![dialog(
            "b-leg-call-id-fedcba@b2bua",
            "b2bua-from-tag-bleg-5544",
            "bob-to-tag-007",
            "sip:alice@example.com",
            "sip:bob@example.com",
            "sip:bob@203.0.113.42:5060;transport=udp",
            4002,
            true,
            1,
        )],
        no_answer_timeout_sec: None,
        bye_disposition: None,
        local_uri: Some("sip:alice@example.com".into()),
        remote_uri: Some("sip:bob@example.com".into()),
        invite_request_uri: Some("sip:bob@example.com".into()),
        pending_invite_txn: None,
        ext: None,
        kind: None,
        adopted: None,
    };

    let mut ext: ExtMap = BTreeMap::new();
    ext.insert(
        "promote-pem".into(),
        serde_json::json!({ "promoted": true, "windowOpen": false, "resyncReinviteCSeq": 8007 }),
    );

    Call {
        call_ref: "worker-0|call-id-deadbeef@example.com|alice-from-tag-001".into(),
        a_leg,
        b_legs: vec![b_leg],
        active_peer: Some(ActivePeer {
            leg_a: "a".into(),
            leg_b: "b-1".into(),
        }),
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
        },
        limiter_entries: vec![CallLimiterState {
            limiter_id: "subscriber:alice@example.com".into(),
            limit: 5,
            origin_window: 1_779_440_000,
            increment_succeeded: Some(true),
        }],
        timers: vec![
            TimerEntry {
                id: "timer-no-answer-a".into(),
                timer_type: TimerType::NoAnswer,
                fire_at: 1_779_440_045_000,
                leg_id: Some("a".into()),
            },
            TimerEntry {
                id: "timer-global-duration".into(),
                timer_type: TimerType::GlobalDuration,
                fire_at: 1_779_443_642_000,
                leg_id: None,
            },
        ],
        cdr_events: vec![
            CdrEvent {
                event_type: CdrEventType::InviteReceived,
                timestamp: 1_779_440_042_000,
                leg_id: "a".into(),
                status_code: None,
                reason: None,
            },
            CdrEvent {
                event_type: CdrEventType::Answer,
                timestamp: 1_779_440_043_200,
                leg_id: "b-1".into(),
                status_code: Some(200),
                reason: None,
            },
        ],
        state: CallModelState::Active,
        created_at: 1_779_440_042_000,
        a_leg_pending_vias: None,
        a_leg_pending_cseq: None,
        tag_map: vec![TagMapping {
            a_tag: "b2bua-to-tag-aleg-9876".into(),
            b_leg_id: "b-1".into(),
            b_tag: "bob-to-tag-007".into(),
        }],
        trace_id: Some("0123456789abcdef0123456789abcdef".into()),
        root_span_id: Some("fedcba9876543210".into()),
        sampled: Some(true),
        worker_index: Some(0),
        topology: Some(CallTopology {
            pri: "worker-0".into(),
            bak: "worker-1".into(),
            gen: 3,
        }),
        emergency: Some(true),
        features: Some(FeatureActivations {
            platform: PlatformActivations {
                max_duration_sec: 3600,
                keepalive: KeepaliveActivation {
                    interval_sec: 30,
                    max_missed: 2,
                },
            },
            refer: None,
            relay_first_18x_to_180: Some(RelayFirst18xTo180Feature {
                strategy: RelayFirst18xStrategy::FakePrack,
            }),
            no_answer_timeout_sec: Some(45),
            call_limiters: None,
        }),
        policy_update_headers: None,
        policy_update_body: None,
        active_rules: Some(vec![
            ActiveRule {
                id: "limit-by-subscriber".into(),
                active: true,
            },
            ActiveRule {
                id: "promote-pem-to-200".into(),
                active: false,
            },
        ]),
        ext: Some(ext),
        message_count: Some(7),
        terminating_refresh_legs: None,
    }
}

// ── proptest strategies ─────────────────────────────────────────────────────

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

fn arb_remote_info() -> impl Strategy<Value = RemoteInfo> {
    ("[a-z0-9.]{1,15}", any::<u16>()).prop_map(|(address, port)| RemoteInfo { address, port })
}
fn arb_host_port() -> impl Strategy<Value = HostPort> {
    ("[a-z0-9.]{1,15}", any::<u16>()).prop_map(|(host, port)| HostPort { host, port })
}
fn arb_invite_handle() -> impl Strategy<Value = InviteTxnHandle> {
    (arb_tag(), arb_bytes(48), arb_host_port()).prop_map(|(branch, original_invite, destination)| {
        InviteTxnHandle {
            branch,
            original_invite,
            destination,
        }
    })
}

fn arb_leg_state() -> impl Strategy<Value = LegState> {
    prop_oneof![
        Just(LegState::Trying),
        Just(LegState::Early),
        Just(LegState::Confirmed),
        Just(LegState::Terminated),
    ]
}
fn arb_leg_disposition() -> impl Strategy<Value = LegDisposition> {
    prop_oneof![
        Just(LegDisposition::Pending),
        Just(LegDisposition::Bridged),
        Just(LegDisposition::Cancelling),
        Just(LegDisposition::Rejected),
    ]
}
fn arb_bye() -> impl Strategy<Value = ByeDisposition> {
    prop_oneof![
        Just(ByeDisposition::ByeSent),
        Just(ByeDisposition::ByeReceived),
        Just(ByeDisposition::ByeConfirmed),
        Just(ByeDisposition::ByeTimeout),
        Just(ByeDisposition::Cancelled),
        Just(ByeDisposition::Rejected),
        Just(ByeDisposition::None),
    ]
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

fn arb_pending_request() -> impl Strategy<Value = PendingRequest> {
    (
        "[A-Z]{3,8}",
        any::<i64>(),
        any::<i64>(),
        proptest::collection::vec("[a-z0-9 /.:;=-]{1,40}", 0..3),
        arb_tag(),
        arb_uri(),
        arb_uri(),
        arb_direction(),
    )
        .prop_map(
            |(method, outbound_cseq, inbound_cseq, source_vias, source_call_id, source_from, source_to, direction)| {
                PendingRequest {
                    method,
                    outbound_cseq,
                    inbound_cseq,
                    source_vias,
                    source_call_id,
                    source_from,
                    source_to,
                    direction,
                }
            },
        )
}

fn arb_dialog() -> impl Strategy<Value = Dialog> {
    let sip = (
        arb_tag(),
        arb_tag(),
        arb_tag(),
        arb_uri(),
        arb_uri(),
        arb_uri(),
        any::<i64>(),
        proptest::collection::vec("[a-z0-9 /.:;=<>-]{1,40}", 0..3),
    )
        .prop_map(
            |(call_id, local_tag, remote_tag, local_uri, remote_uri, remote_target, local_cseq, route_set)| {
                StackDialog {
                    call_id,
                    local_tag,
                    remote_tag,
                    local_uri,
                    remote_uri,
                    remote_target,
                    local_cseq,
                    route_set,
                }
            },
        );
    let ext = (
        proptest::option::of(any::<i64>()),
        proptest::collection::vec(arb_pending_request(), 0..3),
        proptest::option::of(arb_tag()),
        proptest::option::of(arb_invite_handle()),
        proptest::option::of(arb_bytes(80)),
    )
        .prop_map(
            |(remote_cseq, inbound_pending_requests, ack_branch, pending_invite_txn, cached_sdp)| {
                B2buaDialogExt {
                    remote_cseq,
                    inbound_pending_requests,
                    ack_branch,
                    pending_invite_txn,
                    cached_sdp,
                }
            },
        );
    (sip, ext).prop_map(|(sip, ext)| Dialog { sip, ext })
}

fn arb_leg() -> impl Strategy<Value = Leg> {
    (
        "(a|b-[0-9]{1,2})",
        arb_tag(),
        arb_tag(),
        arb_remote_info(),
        arb_leg_state(),
        arb_leg_disposition(),
        proptest::collection::vec(arb_dialog(), 0..3),
        (
            proptest::option::of(0i64..600),
            proptest::option::of(arb_bye()),
        ),
        (
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
        ),
        (
            proptest::option::of(arb_invite_handle()),
            arb_ext(),
            proptest::option::of(arb_leg_kind()),
            proptest::option::of(any::<bool>()),
        ),
    )
        .prop_map(
            |(
                leg_id,
                call_id,
                from_tag,
                source,
                state,
                disposition,
                dialogs,
                (no_answer_timeout_sec, bye_disposition),
                (local_uri, remote_uri, invite_request_uri),
                (pending_invite_txn, ext, kind, adopted),
            )| Leg {
                leg_id,
                call_id,
                from_tag,
                source,
                state,
                disposition,
                dialogs,
                no_answer_timeout_sec,
                bye_disposition,
                local_uri,
                remote_uri,
                invite_request_uri,
                pending_invite_txn,
                ext,
                kind,
                adopted,
            },
        )
}

fn arb_timer() -> impl Strategy<Value = TimerEntry> {
    (arb_tag(), arb_timer_type(), any::<i64>(), proptest::option::of(arb_tag())).prop_map(
        |(id, timer_type, fire_at, leg_id)| TimerEntry {
            id,
            timer_type,
            fire_at,
            leg_id,
        },
    )
}
fn arb_cdr() -> impl Strategy<Value = CdrEvent> {
    (
        arb_cdr_type(),
        any::<i64>(),
        arb_tag(),
        proptest::option::of(100i64..700),
        proptest::option::of("[a-z ]{0,20}"),
    )
        .prop_map(|(event_type, timestamp, leg_id, status_code, reason)| CdrEvent {
            event_type,
            timestamp,
            leg_id,
            status_code,
            reason,
        })
}
fn arb_limiter() -> impl Strategy<Value = CallLimiterState> {
    (arb_tag(), any::<i64>(), any::<i64>(), proptest::option::of(any::<bool>())).prop_map(
        |(limiter_id, limit, origin_window, increment_succeeded)| CallLimiterState {
            limiter_id,
            limit,
            origin_window,
            increment_succeeded,
        },
    )
}
fn arb_tagmap() -> impl Strategy<Value = TagMapping> {
    (arb_tag(), arb_tag(), arb_tag()).prop_map(|(a_tag, b_leg_id, b_tag)| TagMapping {
        a_tag,
        b_leg_id,
        b_tag,
    })
}
fn arb_active_rule() -> impl Strategy<Value = ActiveRule> {
    (arb_tag(), any::<bool>()).prop_map(|(id, active)| ActiveRule { id, active })
}
fn arb_features() -> impl Strategy<Value = FeatureActivations> {
    let platform = (any::<i64>(), any::<i64>(), any::<i64>()).prop_map(
        |(max_duration_sec, interval_sec, max_missed)| PlatformActivations {
            max_duration_sec,
            keepalive: KeepaliveActivation {
                interval_sec,
                max_missed,
            },
        },
    );
    let strat = prop_oneof![
        Just(RelayFirst18xStrategy::DropSdp),
        Just(RelayFirst18xStrategy::KeepSdp),
        Just(RelayFirst18xStrategy::FakePrack),
        Just(RelayFirst18xStrategy::PromotePemTo200),
    ];
    (
        platform,
        proptest::option::of(proptest::option::of(any::<i64>())),
        proptest::option::of(strat),
        proptest::option::of(any::<i64>()),
    )
        .prop_map(|(platform, refer_depth, strategy, no_answer)| FeatureActivations {
            platform,
            refer: refer_depth.map(|max_chain_depth| call::features::ReferFeature { max_chain_depth }),
            relay_first_18x_to_180: strategy
                .map(|strategy| RelayFirst18xTo180Feature { strategy }),
            no_answer_timeout_sec: no_answer,
            call_limiters: None,
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
    )
        .prop_map(|(uri, headers, body)| ALegInviteSnapshot { uri, headers, body })
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
        proptest::option::of((arb_tag(), arb_tag()).prop_map(|(leg_a, leg_b)| ActivePeer { leg_a, leg_b })),
        proptest::option::of("[a-z0-9-]{0,20}"),
        proptest::option::of("[a-z0-9=@.;-]{0,30}"),
        arb_aleg_invite(),
    );
    let collections = (
        proptest::collection::vec(arb_limiter(), 0..3),
        proptest::collection::vec(arb_timer(), 0..4),
        proptest::collection::vec(arb_cdr(), 0..4),
        proptest::collection::vec(arb_tagmap(), 0..3),
    );
    let state = (
        prop_oneof![
            Just(CallModelState::Active),
            Just(CallModelState::Terminating),
            Just(CallModelState::Terminated),
        ],
        any::<i64>(),
        proptest::option::of(proptest::collection::vec("[a-z0-9 /.:;=-]{1,30}", 0..3)),
        proptest::option::of(any::<i64>()),
    );
    let trace = (
        proptest::option::of("[0-9a-f]{32}"),
        proptest::option::of("[0-9a-f]{16}"),
        proptest::option::of(any::<bool>()),
        proptest::option::of(any::<i64>()),
        proptest::option::of(
            (arb_tag(), arb_tag(), any::<i64>())
                .prop_map(|(pri, bak, gen)| CallTopology { pri, bak, gen }),
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
    );

    (head, collections, state, trace, tail).prop_map(
        |(
            (call_ref, a_leg, b_legs, active_peer, callback_context, billing_context, a_leg_invite),
            (limiter_entries, timers, cdr_events, tag_map),
            (state, created_at, a_leg_pending_vias, a_leg_pending_cseq),
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
            ),
        )| Call {
            call_ref,
            a_leg,
            b_legs,
            active_peer,
            callback_context,
            billing_context,
            a_leg_invite,
            limiter_entries,
            timers,
            cdr_events,
            state,
            created_at,
            a_leg_pending_vias,
            a_leg_pending_cseq,
            tag_map,
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
        },
    )
}
