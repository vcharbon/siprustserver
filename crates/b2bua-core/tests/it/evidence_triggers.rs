//! The evidence rule's triggers beyond an INVITE final (ADR-0055 §4): a final
//! of any method above its dialog's sequence, a final at a number this side
//! re-used, an ACK of an INVITE the record holds unanswered and a 2xx naming a
//! leg the record lacks. Each is owed what
//! RFC 3261 requires of it and ends the call over every dialog the record or
//! the evidence names; none touches a dialog the evidence proves ended.

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{CriticalStateEffect, HandlerResult, OutboundBody};
use b2bua_core::rules::{
    default_rules, pick_ranked, ActionExecutor, RuleAction, RuleCall, RuleContext,
};
use b2bua_core::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use call::helpers::Resolved;
use call::{Dialog, DialogId, DialogState, Direction, LegState};
use sip_ids::{SigningKey, TurnIds};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::evidence_rules::{bridged, requests_to};

/// The callee's sequence the record holds (`bridged`).
const RECORDED: u32 = 3;

fn parse(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("a message")
}

/// The callee's final `status` to a `method` of CSeq `cseq` on its dialog,
/// under the Via branch `branch`.
fn final_from_bob(status: u16, method: &str, cseq: u32, branch: &str) -> CallEvent {
    let raw = format!(
        "SIP/2.0 {status} Whatever\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch={branch}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: {cseq} {method}\r\n\
Content-Length: 0\r\n\r\n"
    );
    CallEvent::sip(parse(&raw), "10.0.0.2:5070".parse().unwrap())
}

/// The callee's dialog, as the engine resolves a message on it.
fn on_bobs_dialog() -> Resolved {
    let dialog = DialogId { our_tag: "svc".into(), peer_tag: "bobtag".into() };
    Resolved { dialog: Some(dialog), record: None }
}

/// The rule that claims `event` from `leg` resolved as `resolved`, and the
/// turn it makes under `config`.
fn turn_on(
    call: &call::Call,
    event: &CallEvent,
    leg: &str,
    resolved: &Resolved,
    config: &B2buaConfig,
) -> (Option<&'static str>, Option<HandlerResult>) {
    let direction = if leg == "a" { Direction::FromA } else { Direction::FromB };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: leg,
        direction,
        now_ms: 0,
        config,
        discharged: None,
        resolved,
    };
    // The evidence stage reads the event before the rule chain does.
    let evidence = b2bua_core::rules::evidence::evidence_rules(false);
    let rules = default_rules();
    let first = pick_ranked(&evidence, call, &ctx).into_iter().next();
    let Some(rule) = first.or_else(|| pick_ranked(&rules, call, &ctx).into_iter().next()) else {
        return (None, None);
    };
    let actions: Vec<RuleAction> = (rule.handle)(&ctx).map(|r| r.actions).unwrap_or_default();
    let mut ids = TurnScope::of(TurnIds::fixed(1));
    let mut exec = ActionExecutor {
        owners: b2bua_core::owners::Owners::none(),
        config,
        turn: &mut ids,
        now_ms: 0,
    };
    (Some(rule.id), Some(exec.execute(&actions, call, &ctx)))
}

/// A call ringing at `b-1`: the callee's early dialog `svc`/`bobtag`, the
/// caller not answered.
fn ringing() -> call::Call {
    let mut call = bridged();
    call.a_leg.dialogs = Default::default();
    call.a_leg.own_tag = None;
    *call.a_leg.invite_final_sent_mut() = None;
    call = call::helpers::update_leg(call, "b-1", |leg| {
        leg.dialogs = Default::default();
        let mut d = Dialog::new("svc", "bobtag", DialogState::Early, 1);
        d.remote_target = "sip:bob@10.0.0.2:5070".into();
        leg.dialogs.insert(d);
        if let Some(invite) = leg.outbound_invite_mut() {
            invite.final_received = None;
        }
    });
    assert_eq!(call.a_leg.state(), LegState::Trying);
    call
}

/// Whether `result` sends any response toward `leg`.
fn responds_to(result: &HandlerResult, leg: &str) -> Vec<u16> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .filter_map(|e| match &e.body {
            OutboundBody::Response(r) => Some(r.status()),
            _ => None,
        })
        .collect()
}

/// The callee answers a BYE the record never saw sent: its dialog is over, so
/// the call ends with a BYE to the caller alone.
#[test]
fn a_bye_final_beyond_the_record_ends_the_call_and_byes_the_other_dialog_alone() {
    let call = bridged();
    let event = final_from_bob(200, "BYE", RECORDED + 1, "z9hG4bKgone");
    let (rule, result) = turn_on(&call, &event, "b-1", &on_bobs_dialog(), &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    assert!(requests_to(&result, "b-1").is_empty(), "the BYE's dialog is over");
    assert!(requests_to(&result, "a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
    assert_eq!(result.call.state, call::CallModelState::Terminating);
}

/// A final to an INFO the record never saw sent: the callee's dialog is BYEd
/// past the number the peer saw (RFC 3261 §12.2.1.1), and the caller's too.
#[test]
fn an_info_final_beyond_the_record_byes_every_dialog_past_its_number() {
    let call = bridged();
    let event = final_from_bob(481, "INFO", RECORDED + 1, "z9hG4bKgone");
    let (rule, result) = turn_on(&call, &event, "b-1", &on_bobs_dialog(), &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let to_bob = requests_to(&result, "b-1");
    assert!(to_bob.iter().any(|(m, c)| m == "BYE" && *c > RECORDED + 1), "{to_bob:?}");
    assert!(requests_to(&result, "a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
}

/// This side re-used a number another owner sent: a final at that number
/// under another branch than the request the record holds open answers the
/// other owner's request.
#[test]
fn a_final_at_a_reused_number_under_another_branch_is_evidence() {
    let mut call = bridged();
    let sent = call::SentRequest {
        branch: "z9hG4bKmine".into(),
        image: Vec::new(),
        destination: call::HostPort { host: "10.0.0.2".into(), port: 5070 },
    };
    let record =
        call::RequestRecord::own("OPTIONS", i64::from(RECORDED), sent, 32_000, Default::default());
    call = call::helpers::update_leg(call, "b-1", |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            d.requests.push(record);
        }
    });
    let event = final_from_bob(200, "OPTIONS", RECORDED, "z9hG4bKother");
    let (rule, _) = turn_on(&call, &event, "b-1", &on_bobs_dialog(), &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-final"));
}

/// This side re-used the number of a re-INVITE another owner sent, and has
/// sent past it since: the 2xx to the other owner's re-INVITE is ACKed at its
/// own number, the one that 2xx names (RFC 3261 §13.2.2.4), not at this
/// side's latest.
#[test]
fn the_ack_of_a_2xx_at_a_reused_number_takes_that_number() {
    let reinvite = format!(
        "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKmine\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: {RECORDED} INVITE\r\n\
Contact: <sip:b2bua@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let sent = call::SentRequest {
        branch: "z9hG4bKmine".into(),
        image: reinvite.into_bytes(),
        destination: call::HostPort { host: "10.0.0.2".into(), port: 5070 },
    };
    let call = call::helpers::update_leg(bridged(), "b-1", |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            d.sent_invite = Some(call::testkit::own_invite(i64::from(RECORDED), Some(sent)));
            d.our_cseq = i64::from(RECORDED + 1);
        }
    });
    let event = final_from_bob(200, "INVITE", RECORDED, "z9hG4bKother");
    let (rule, result) = turn_on(&call, &event, "b-1", &on_bobs_dialog(), &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let ack = ack_to(&result, "b-1").expect("the 2xx is ACKed");
    assert_eq!(ack.cseq().seq(), RECORDED, "at the number the 2xx names");
}

/// A final at or below the sequence that answers no request held open at its
/// number is a stray, not evidence.
#[test]
fn a_stray_final_at_or_below_the_sequence_is_no_evidence() {
    let call = bridged();
    let event = final_from_bob(200, "OPTIONS", RECORDED, "z9hG4bKother");
    let (rule, _) = turn_on(&call, &event, "b-1", &on_bobs_dialog(), &B2buaConfig::default());
    assert_ne!(rule, Some("evidence-final"));
}

/// The caller ACKs a 2xx the record never saw sent: her INVITE was answered,
/// so it draws no final here — its transaction is relinquished — and the
/// dialog the ACK names is BYEd under the ACK's tags.
#[test]
fn an_ack_of_the_unanswered_caller_invite_byes_her_dialog_under_its_tags() {
    let call = ringing();
    let raw = "ACK sip:b2bua@10.0.0.9:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKack\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@host>;tag=atag\r\n\
To: <sip:bob@host>;tag=shown\r\n\
Call-ID: c1@alice\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n";
    let event = CallEvent::sip(parse(raw), "127.0.0.1:5060".parse().unwrap());
    let (rule, result) = turn_on(&call, &event, "a", &Resolved::default(), &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-ack"));
    let result = result.expect("a turn");
    assert_eq!(result.call.a_leg.invite_final_sent(), Some(200), "answered elsewhere");
    assert!(responds_to(&result, "a").is_empty(), "no final follows the 2xx");
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::RelinquishAnsweredInvite { .. })),
        "the caller's INVITE transaction is relinquished"
    );
    let bye = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "BYE" => Some(r.clone()),
            _ => None,
        })
        .expect("the caller's BYE");
    assert_eq!(bye.from().tag(), Some("shown"), "under the tag the ACK names");
    assert_eq!(bye.to().tag(), Some("atag"));
}

/// An ACK of her INVITE naming no dialog proves no answer: no 2xx draws an
/// ACK without the To-tag (RFC 3261 §13.2.2.4).
#[test]
fn a_tagless_ack_of_the_unanswered_caller_invite_is_no_evidence() {
    let raw = "ACK sip:b2bua@10.0.0.9:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKack\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@host>;tag=atag\r\n\
To: <sip:bob@host>\r\n\
Call-ID: c1@alice\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n";
    let event = CallEvent::sip(parse(raw), "127.0.0.1:5060".parse().unwrap());
    let (rule, _) = turn_on(&ringing(), &event, "a", &Resolved::default(), &B2buaConfig::default());
    assert_ne!(rule, Some("evidence-ack"));
}

/// A 2xx whose signed via-extension names a leg the record lacks: ACKed and
/// BYEd from its own headers, and the call ended.
#[test]
fn a_2xx_naming_a_leg_the_record_lacks_is_acked_and_byed() {
    let key = SigningKey::new(&[7u8; 32]).expect("a key");
    let config = B2buaConfig { cluster_key: Some(key.clone()), ..B2buaConfig::default() };
    let call = bridged();
    let facts = RequestFacts { method: "INVITE", uri: "sip:carol@10.0.0.3:5070", offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: "10.0.0.9",
            local_port: 5060,
            call_ref: &call.call_ref,
            leg: "b-9",
            incarnation_mark: call.incarnation_mark(),
            is_emergency: false,
            other_owner: None,
            request: Some((facts, &key)),
        },
        "z9hG4bKcarol".to_string(),
    );
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: {}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc9\r\n\
To: <sip:carol@10.0.0.3>;tag=caroltag\r\n\
Call-ID: b9cid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:carol@10.0.0.3:5070>\r\n\
Content-Length: 0\r\n\r\n",
        sip_message::header::HeaderValue::to_wire(&via)
    );
    let event = CallEvent::sip(parse(&raw), "10.0.0.3:5070".parse().unwrap());
    let (rule, result) = turn_on(&call, &event, "b-9", &Resolved::default(), &config);
    assert_eq!(rule, Some("evidence-unknown-leg-2xx"));
    let result = result.expect("a turn");
    let to_carol: Vec<String> = requests_to(&result, "b-9").into_iter().map(|(m, _)| m).collect();
    assert_eq!(to_carol, ["ACK", "BYE"]);
    assert_eq!(result.call.state, call::CallModelState::Terminating);
}

/// A 2xx of a leg the record lacks, on a ringing call whose caller hears a
/// media leg's early media under her primary tag: another owner relayed that
/// 2xx under the tag fronting the callee's dialog, as the primary is the media
/// leg's own session (RFC 3264 §8). The non-2xx she is owed, and the hold,
/// take that fronting tag (ADR-0055 §4).
#[test]
fn the_hold_of_a_leg_the_record_lacks_takes_the_fronting_tag_beside_early_media() {
    let key = SigningKey::new(&[7u8; 32]).expect("a key");
    let config = B2buaConfig { cluster_key: Some(key.clone()), ..B2buaConfig::default() };
    let mut call = ringing();
    let mut primary = Dialog::new("media-face", "atag", DialogState::Early, 1);
    primary.remote_target = "sip:alice@127.0.0.1:5060".into();
    primary.shows_media = Some("m-1".into());
    call.a_leg.dialogs.insert(primary);
    call.a_leg.own_tag = Some("media-face".into());
    let facts = RequestFacts { method: "INVITE", uri: "sip:carol@10.0.0.3:5070", offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: "10.0.0.9",
            local_port: 5060,
            call_ref: &call.call_ref,
            leg: "b-9",
            incarnation_mark: call.incarnation_mark(),
            is_emergency: false,
            other_owner: None,
            request: Some((facts, &key)),
        },
        "z9hG4bKcarol".to_string(),
    );
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: {}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc9\r\n\
To: <sip:carol@10.0.0.3>;tag=caroltag\r\n\
Call-ID: b9cid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:carol@10.0.0.3:5070>\r\n\
Content-Length: 0\r\n\r\n",
        sip_message::header::HeaderValue::to_wire(&via)
    );
    let event = CallEvent::sip(parse(&raw), "10.0.0.3:5070".parse().unwrap());
    let (rule, result) = turn_on(&call, &event, "b-9", &Resolved::default(), &config);
    assert_eq!(rule, Some("evidence-unknown-leg-2xx"));
    let result = result.expect("a turn");
    let fronting = b2bua_core::caller_tags::fronting(&call, "b-9", "caroltag");
    let to_her = result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .find_map(|e| match &e.body {
            OutboundBody::Response(r) if r.status() >= 300 => Some(r.clone()),
            _ => None,
        })
        .expect("the non-2xx she is owed");
    assert_eq!(to_her.to().tag(), Some(fronting.as_str()), "under the fronting tag");
    assert!(
        call::helpers::caller_dialog(&result.call, &fronting).is_some(),
        "the held dialog is the fronting one, BYEd on her ACK or at 64·T1"
    );
}

/// The callee's 2xx to a re-INVITE another owner sent, on the early dialog
/// the record holds: the re-INVITE proves the dialog confirmed and the
/// dialled INVITE answered. The 2xx is ACKed, the dialog BYEd past its
/// number, and no CANCEL follows the answer.
#[test]
fn a_2xx_above_the_dialled_number_on_an_early_dialog_confirms_it_and_byes_it() {
    let event = final_from_bob(200, "INVITE", 2, "z9hG4bKreinvite");
    let config = B2buaConfig::default();
    let (rule, result) = turn_on(&ringing(), &event, "b-1", &on_bobs_dialog(), &config);
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let to_bob = requests_to(&result, "b-1");
    assert_eq!(to_bob.first(), Some(&("ACK".to_string(), 2)), "{to_bob:?}");
    assert!(to_bob.iter().any(|(m, c)| m == "BYE" && *c == 3), "{to_bob:?}");
    assert!(to_bob.iter().all(|(m, _)| m != "CANCEL"), "{to_bob:?}");
}

/// The transfer target's 2xx at a CSeq above the INVITE that dialled it
/// answers a later request of its early-held dialog — a re-INVITE another
/// owner sent after the target's answer: the transfer machine does not read
/// it as the target answering its INVITE, and the evidence rule claims it.
#[test]
fn a_transfer_targets_2xx_above_its_dialled_number_is_evidence_not_its_answer() {
    let mut call = bridged();
    let mut target = call::helpers::find_leg(&call, "b-1").expect("the referrer").clone();
    target.leg_id = "b-2".into();
    target.dialogs = Default::default();
    target.dialogs.insert(Dialog::new("svc", "bobtag", DialogState::Early, 1));
    if let Some(invite) = target.outbound_invite_mut() {
        invite.final_received = None;
    }
    call = call::helpers::add_b_leg(call, target);
    call.transfer = Some(call::TransferState {
        phase: call::TransferPhase::CRinging,
        referrer_leg_id: "b-1".into(),
        refer_to_uri: "sip:carol@10.0.0.3".into(),
        effective_refer_to_uri: None,
        callback_context: None,
        c_leg_id: Some("b-2".into()),
        refer_cseq: Some(2),
        notify_event: "refer".into(),
        started_at_ms: 0,
        last_c_leg_provisional_status: Some(180),
        c_initial_sdp: None,
    });
    b2bua_core::rules::refer_transfer::project_cursor(&mut call);
    let event = final_from_bob(200, "INVITE", 2, "z9hG4bKreinvite");
    let config = B2buaConfig::default();
    let (rule, _) = turn_on(&call, &event, "b-2", &on_bobs_dialog(), &config);
    assert_eq!(rule, Some("evidence-final"));
}

/// The evidence stage reads the event before any rule: a service rule that
/// claims every response on the callee's leg does not pre-empt a final only
/// a lost write explains (ADR-0055 §4) — the evidence ends the call. With no
/// evidence stage the service rule would have claimed it.
#[test]
fn a_service_rule_on_the_same_leg_does_not_pre_empt_evidence() {
    use b2bua_core::obligations::ObligationSet;
    use b2bua_core::rules::{execute_rules, execute_turn, Match, RuleDefinition};
    use b2bua_sdk::model::{RuleHandleResult, SERVICE_LAYER};

    let greedy = RuleDefinition::core(
        "greedy-service",
        SERVICE_LAYER,
        &[],
        Match::response().direction(Direction::FromB),
        |_| Some(RuleHandleResult::new(vec![])),
    );
    let mut rules = default_rules();
    rules.push(greedy);
    let call = bridged();
    let event = final_from_bob(200, "BYE", RECORDED + 1, "z9hG4bKgone");
    let config = B2buaConfig::default();
    let resolved = on_bobs_dialog();
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &resolved,
    };
    let run = |stage: bool| {
        let mut ids = TurnScope::of(TurnIds::fixed(1));
        let owners = b2bua_core::owners::Owners::none();
        let mut exec = ActionExecutor { owners, config: &config, turn: &mut ids, now_ms: 0 };
        let obligations = ObligationSet::core();
        if stage {
            execute_turn(&rules, &call, &ctx, false, &mut exec, &obligations)
        } else {
            execute_rules(&rules, &call, &ctx, &mut exec, &obligations)
        }
    };
    assert_eq!(run(true).call.state, call::CallModelState::Terminating, "evidence ends the call");
    assert_eq!(run(false).call.state, call.state, "the service rule alone claims the final");
}

/// A 2xx carrying an offer — the INVITE that dialled the leg made none —
/// ACKed on this stack's own account with no body given: the ACK still
/// answers the offer, rejecting every stream (RFC 3261 §13.2.2.4, RFC 3264
/// §6), never bare.
#[test]
fn an_authored_bare_ack_of_an_offering_2xx_answers_it_rejecting_every_stream() {
    let offer = "v=0\r\no=bob 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\n\
                 m=audio 20000 RTP/AVP 0\r\n";
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKdialled\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{offer}",
        offer.len()
    );
    let event = CallEvent::sip(parse(&raw), "10.0.0.2:5070".parse().unwrap());
    let call = ringing();
    let config = B2buaConfig::default();
    let resolved = on_bobs_dialog();
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &resolved,
    };
    let mut ids = TurnScope::of(TurnIds::fixed(1));
    let owners = b2bua_core::owners::Owners::none();
    let mut exec = ActionExecutor { owners, config: &config, turn: &mut ids, now_ms: 0 };
    let actions = [
        RuleAction::ConfirmDialog { leg_id: "b-1".into() },
        RuleAction::AckLeg { leg_id: "b-1".into(), body: None },
    ];
    let result = exec.execute(&actions, &call, &ctx);
    let ack = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "ACK" => Some(r.clone()),
            _ => None,
        })
        .expect("the ACK");
    let answer = String::from_utf8_lossy(ack.body()).to_string();
    assert!(answer.contains("m=audio 0 "), "every stream rejected: {answer}");
}

/// The caller's re-INVITE, relayed to bob at the next number of his dialog
/// and still open in the record: bob's 2xx left with another owner.
fn relaying_reinvite() -> call::Call {
    let reinvite = format!(
        "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKrelayed\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: {} INVITE\r\n\
Contact: <sip:b2bua@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n",
        RECORDED + 1
    );
    let mut relay = call::RequestRecord::relayed("INVITE", i64::from(RECORDED + 1), 2);
    relay.source_vias = vec!["SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKre".into()];
    relay.sent = Some(call::SentRequest {
        branch: "z9hG4bKrelayed".into(),
        image: reinvite.into_bytes(),
        destination: call::HostPort { host: "10.0.0.2".into(), port: 5070 },
    });
    let mut call = bridged();
    call.active_peer = Some(call::ActivePeer { leg_a: "a".into(), leg_b: "b-1".into() });
    call::helpers::update_leg(call, "b-1", |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            d.our_cseq = i64::from(RECORDED + 1);
            d.sent_invite = Some(relay);
        }
    })
}

/// The ACK `result` sends toward `leg`, composed or repeated as the bytes it
/// left as, if any.
fn ack_to(result: &HandlerResult, leg: &str) -> Option<sip_message::SipRequest> {
    result.effects.outbound.iter().filter(|e| e.leg_id.as_deref() == Some(leg)).find_map(|e| {
        let request = match &e.body {
            OutboundBody::Request(r) => r.clone(),
            OutboundBody::Datagram(d) => match CustomParser::new().parse(d.wire().0).ok()? {
                SipMessage::Request(r) => r,
                SipMessage::Response(_) => return None,
            },
            OutboundBody::Response(_) => return None,
        };
        (request.method().as_str() == "ACK").then_some(request)
    })
}

/// The caller ACKs the 2xx another owner relayed her for her re-INVITE: the
/// ACK is the one bob's 2xx is owed (RFC 3261 §13.2.2.4). It goes to bob at
/// the relayed INVITE's number, and a repeat of bob's 2xx draws it again;
/// the call then ends on the evidence.
#[test]
fn an_ack_of_a_relayed_reinvite_keeps_the_callees_2xx_acked() {
    let call = relaying_reinvite();
    let raw = "ACK sip:b2bua@10.0.0.9:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKreack\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@host>;tag=atag\r\n\
To: <sip:bob@host>;tag=b2bua-a\r\n\
Call-ID: c1@alice\r\n\
CSeq: 2 ACK\r\n\
Content-Length: 0\r\n\r\n";
    let event = CallEvent::sip(parse(raw), "127.0.0.1:5060".parse().unwrap());
    let on_hers = Resolved {
        dialog: Some(DialogId { our_tag: "b2bua-a".into(), peer_tag: "atag".into() }),
        record: None,
    };
    let (rule, result) = turn_on(&call, &event, "a", &on_hers, &B2buaConfig::default());
    assert_eq!(rule, Some("evidence-ack"));
    let result = result.expect("a turn");
    let ack = ack_to(&result, "b-1").expect("bob's 2xx is ACKed");
    assert_eq!(ack.cseq().seq(), RECORDED + 1, "at the relayed INVITE's number");

    let repeat = final_from_bob(200, "INVITE", RECORDED + 1, "z9hG4bKrelayed");
    let config = B2buaConfig::default();
    let (_, again) = turn_on(&result.call, &repeat, "b-1", &on_bobs_dialog(), &config);
    let again = again.expect("a turn on the repeat");
    let reack = ack_to(&again, "b-1").expect("the repeat of bob's 2xx is ACKed again");
    assert_eq!(reack.cseq().seq(), RECORDED + 1);
}
