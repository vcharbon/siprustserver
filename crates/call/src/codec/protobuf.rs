//! `model::Call` ↔ `proto::wire::Call` field-mapping shims — the Rust analogue
//! of the `toProtoObject` / `fromProtoObject` encode/decode mappers in
//! `src/call/codec/protobuf.ts`, plus the `*IsNull` / `*Present` side-channels
//! and the JSON-string carries the proto schema's comments describe.
//!
//! ## Why this shim is *smaller* than the TS one
//!
//! The TS source carries a forest of `*IsNull` / `*Present` / `*Json` helper
//! flags because, under Effect `Schema` + plain JS objects, proto3's
//! absent/null/empty collapse and its lack of a union type erase distinctions
//! the model needs back. Several of those distinctions are already encoded by
//! the Rust *type* on this side of the wire, so the corresponding shim work
//! disappears:
//!
//! - **`Option<T>` already is "absent vs present".** Where the TS mapper guards
//!   `x !== undefined` before setting a field and re-reads the guard on decode,
//!   the Rust side just maps `Some`/`None` to a proto `optional` field — which is
//!   exactly `Option<T>` after codegen. No helper flag.
//! - **`Option<PolicyUpdateBody>` collapses the three-way body union.** The TS
//!   `policyUpdateBody` is `optional(NullOr(Uint8Array))` (absent / null / bytes)
//!   carried by `policyUpdateBody` + `policyUpdateBodyIsNull`. Here it is one
//!   `Option<PolicyUpdateBody>`: `None` = absent, `Some(Empty)` = the source's
//!   `null` (force-empty body), `Some(Bytes)` = substitute. The mapping below
//!   *does* still drive `policy_update_body_is_null`, because that is the wire
//!   contract the schema fixes — but the Rust value it derives from is a single
//!   typed field, not two loose ones.
//! - **`serde_json::Value` is the union for the opaque carries.** `features`,
//!   `ext` (Call + Leg), `policyUpdateHeaders`, and the best-effort
//!   `pendingInviteTxn` handle ride proto `string` fields as JSON, exactly as in
//!   TS — but the round-trip is `serde_json::to_string`/`from_str` over typed
//!   Rust values, so there is no hand-rolled per-field (de)serialisation.
//!
//! Two proto messages are **schema-reserved but unused by the codec**, matching
//! the TS mapper exactly: `ActiveRule`'s `params{Present,Json}` (the production
//! `ActiveRule` is `{id, active}` — `protobuf.ts`'s `encodeActiveRule` writes
//! only those two, and the Rust [`crate::model::ActiveRule`] has no `params`
//! field either) and the top-level `ruleState{,Present}` (the TS mapper never
//! reads or writes it; there is no `rule_state` on the Rust `Call`). Both are
//! left at their proto3 defaults on encode and ignored on decode, so the wire
//! tag stays reserved (ADR-0011) without inventing model state.
//!
//! ## Integer width
//!
//! The schema mirrors the JS number domain: cseqs / ports / counts are `int32`
//! and timestamps are `double`. The Rust model uses `i64` (and `u16` for ports),
//! so the mappers narrow on encode (`as i32` / `as f64`) and widen on decode.
//! Epoch-ms timestamps (`created_at`, `fire_at`, …) ride `double` and round-trip
//! exactly while under 2^53; realistic SIP cseqs / ports / limits sit well inside
//! `int32`. This is the same lossy-at-the-extremes contract the TS `int32` fields
//! carry — the fixture pool (and production) only ever populate representable
//! values.

use crate::model::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, B2buaDialogExt, ByeDisposition, Call,
    CallLimiterState, CallModelState, CdrEvent, CdrEventType, Dialog, Direction, ExtMap,
    InviteTxnHandle, Leg, LegDisposition, LegKind, LegState, PendingRequest, PolicyUpdateBody,
    RemoteInfo, SipHeader, StackDialog, TagMapping, TimerEntry, TimerType,
};
use crate::proto::wire;

use super::CallDecodeError;

// ── Small enum ⇄ wire-string helpers ────────────────────────────────────────
//
// The model's closed-union enums serialise to the same kebab/lower/snake-case
// tokens the TS `Schema.Literals` produce (the proto carries them as bare
// `string`). Encoding is total; decoding is fallible (a bad token is a corrupt
// body) and surfaces as `CallDecodeError::Decode`.

fn leg_state_str(s: LegState) -> &'static str {
    match s {
        LegState::Trying => "trying",
        LegState::Early => "early",
        LegState::Confirmed => "confirmed",
        LegState::Terminated => "terminated",
    }
}
fn leg_state_from(s: &str) -> Result<LegState, CallDecodeError> {
    Ok(match s {
        "trying" => LegState::Trying,
        "early" => LegState::Early,
        "confirmed" => LegState::Confirmed,
        "terminated" => LegState::Terminated,
        other => return Err(bad("Leg.state", other)),
    })
}

fn leg_disposition_str(d: LegDisposition) -> &'static str {
    match d {
        LegDisposition::Pending => "pending",
        LegDisposition::Bridged => "bridged",
        LegDisposition::Cancelling => "cancelling",
        LegDisposition::Rejected => "rejected",
    }
}
fn leg_disposition_from(s: &str) -> Result<LegDisposition, CallDecodeError> {
    Ok(match s {
        "pending" => LegDisposition::Pending,
        "bridged" => LegDisposition::Bridged,
        "cancelling" => LegDisposition::Cancelling,
        "rejected" => LegDisposition::Rejected,
        other => return Err(bad("Leg.disposition", other)),
    })
}

fn bye_disposition_str(b: ByeDisposition) -> &'static str {
    match b {
        ByeDisposition::ByeSent => "bye_sent",
        ByeDisposition::ByeReceived => "bye_received",
        ByeDisposition::ByeConfirmed => "bye_confirmed",
        ByeDisposition::ByeTimeout => "bye_timeout",
        ByeDisposition::Cancelled => "cancelled",
        ByeDisposition::Rejected => "rejected",
        ByeDisposition::None => "none",
    }
}
fn bye_disposition_from(s: &str) -> Result<ByeDisposition, CallDecodeError> {
    Ok(match s {
        "bye_sent" => ByeDisposition::ByeSent,
        "bye_received" => ByeDisposition::ByeReceived,
        "bye_confirmed" => ByeDisposition::ByeConfirmed,
        "bye_timeout" => ByeDisposition::ByeTimeout,
        "cancelled" => ByeDisposition::Cancelled,
        "rejected" => ByeDisposition::Rejected,
        "none" => ByeDisposition::None,
        other => return Err(bad("Leg.byeDisposition", other)),
    })
}

fn leg_kind_str(k: LegKind) -> &'static str {
    match k {
        LegKind::A => "a",
        LegKind::Destination => "destination",
        LegKind::Media => "media",
        LegKind::TransferTarget => "transfer-target",
    }
}
fn leg_kind_from(s: &str) -> Result<LegKind, CallDecodeError> {
    Ok(match s {
        "a" => LegKind::A,
        "destination" => LegKind::Destination,
        "media" => LegKind::Media,
        "transfer-target" => LegKind::TransferTarget,
        other => return Err(bad("Leg.kind", other)),
    })
}

fn direction_str(d: Direction) -> &'static str {
    match d {
        Direction::FromA => "from-a",
        Direction::FromB => "from-b",
    }
}
fn direction_from(s: &str) -> Result<Direction, CallDecodeError> {
    Ok(match s {
        "from-a" => Direction::FromA,
        "from-b" => Direction::FromB,
        other => return Err(bad("PendingRequest.direction", other)),
    })
}

fn timer_type_str(t: TimerType) -> &'static str {
    match t {
        TimerType::NoAnswer => "no_answer",
        TimerType::SetupTimeout => "setup_timeout",
        TimerType::GlobalDuration => "global_duration",
        TimerType::LimiterRefresh => "limiter_refresh",
        TimerType::Keepalive => "keepalive",
        TimerType::KeepaliveTimeout => "keepalive_timeout",
        TimerType::AckRetransmit => "ack_retransmit",
        TimerType::AckTimeout => "ack_timeout",
        TimerType::TerminatingTimeout => "terminating_timeout",
        TimerType::ReferSubscriptionExpiry => "refer_subscription_expiry",
        TimerType::ReferReinviteAnswer => "refer_reinvite_answer",
        TimerType::ReferOverallSafety => "refer_overall_safety",
    }
}
fn timer_type_from(s: &str) -> Result<TimerType, CallDecodeError> {
    Ok(match s {
        "no_answer" => TimerType::NoAnswer,
        "setup_timeout" => TimerType::SetupTimeout,
        "global_duration" => TimerType::GlobalDuration,
        "limiter_refresh" => TimerType::LimiterRefresh,
        "keepalive" => TimerType::Keepalive,
        "keepalive_timeout" => TimerType::KeepaliveTimeout,
        "ack_retransmit" => TimerType::AckRetransmit,
        "ack_timeout" => TimerType::AckTimeout,
        "terminating_timeout" => TimerType::TerminatingTimeout,
        "refer_subscription_expiry" => TimerType::ReferSubscriptionExpiry,
        "refer_reinvite_answer" => TimerType::ReferReinviteAnswer,
        "refer_overall_safety" => TimerType::ReferOverallSafety,
        other => return Err(bad("TimerEntry.type", other)),
    })
}

fn cdr_type_str(t: CdrEventType) -> &'static str {
    match t {
        CdrEventType::InviteReceived => "invite_received",
        CdrEventType::InviteSent => "invite_sent",
        CdrEventType::Provisional => "provisional",
        CdrEventType::Answer => "answer",
        CdrEventType::Bye => "bye",
        CdrEventType::Cancel => "cancel",
        CdrEventType::Timeout => "timeout",
        CdrEventType::Reject => "reject",
    }
}
fn cdr_type_from(s: &str) -> Result<CdrEventType, CallDecodeError> {
    Ok(match s {
        "invite_received" => CdrEventType::InviteReceived,
        "invite_sent" => CdrEventType::InviteSent,
        "provisional" => CdrEventType::Provisional,
        "answer" => CdrEventType::Answer,
        "bye" => CdrEventType::Bye,
        "cancel" => CdrEventType::Cancel,
        "timeout" => CdrEventType::Timeout,
        "reject" => CdrEventType::Reject,
        other => return Err(bad("CdrEvent.type", other)),
    })
}

fn call_state_str(s: CallModelState) -> &'static str {
    match s {
        CallModelState::Active => "active",
        CallModelState::Terminating => "terminating",
        CallModelState::Terminated => "terminated",
    }
}
fn call_state_from(s: &str) -> Result<CallModelState, CallDecodeError> {
    Ok(match s {
        "active" => CallModelState::Active,
        "terminating" => CallModelState::Terminating,
        "terminated" => CallModelState::Terminated,
        other => return Err(bad("Call.state", other)),
    })
}

fn bad(field: &str, token: &str) -> CallDecodeError {
    CallDecodeError::Decode(format!("invalid wire token {token:?} for {field}"))
}

// JSON-string carries: serialise a typed Rust value to a proto `string` and back.
fn to_json_string<T: serde::Serialize>(v: &T) -> String {
    // The Call subtree contains only JSON-expressible types (the same closed set
    // that round-trips through msgpack), so this is infallible in practice.
    serde_json::to_string(v).expect("Call sub-value JSON serialization is infallible")
}
fn from_json_string<T: serde::de::DeserializeOwned>(
    field: &str,
    s: &str,
) -> Result<T, CallDecodeError> {
    serde_json::from_str(s)
        .map_err(|e| CallDecodeError::Decode(format!("{field} JSON carry parse failed: {e}")))
}

// ── Dialog ──────────────────────────────────────────────────────────────────

fn encode_dialog(d: &Dialog) -> wire::Dialog {
    wire::Dialog {
        sip: Some(wire::StackDialog {
            call_id: d.sip.call_id.clone(),
            local_tag: d.sip.local_tag.clone(),
            remote_tag: d.sip.remote_tag.clone(),
            local_uri: d.sip.local_uri.clone(),
            remote_uri: d.sip.remote_uri.clone(),
            remote_target: d.sip.remote_target.clone(),
            local_c_seq: d.sip.local_cseq as i32,
            route_set: d.sip.route_set.clone(),
        }),
        ext: Some(wire::B2buaDialogExt {
            // `remote_cseq: Option<i64>` — `None` is the source's `null`, carried
            // by the explicit null bit; `Some` rides the value field.
            remote_c_seq: d.ext.remote_cseq.map(|n| n as i32),
            remote_c_seq_is_null: if d.ext.remote_cseq.is_none() {
                Some(true)
            } else {
                None
            },
            inbound_pending_requests: d
                .ext
                .inbound_pending_requests
                .iter()
                .map(encode_pending_request)
                .collect(),
            ack_branch: d.ext.ack_branch.clone(),
            // Best-effort handle, JSON (`InviteTxnHandle` incl. its raw INVITE).
            pending_invite_txn_json: d.ext.pending_invite_txn.as_ref().map(to_json_string),
            cached_sdp: d.ext.cached_sdp.clone(),
        }),
    }
}

fn decode_dialog(d: wire::Dialog) -> Result<Dialog, CallDecodeError> {
    let sip = d.sip.ok_or_else(|| missing("Dialog.sip"))?;
    let ext = d.ext.ok_or_else(|| missing("Dialog.ext"))?;
    Ok(Dialog {
        sip: StackDialog {
            call_id: sip.call_id,
            local_tag: sip.local_tag,
            remote_tag: sip.remote_tag,
            local_uri: sip.local_uri,
            remote_uri: sip.remote_uri,
            remote_target: sip.remote_target,
            local_cseq: sip.local_c_seq as i64,
            route_set: sip.route_set,
        },
        ext: B2buaDialogExt {
            // Mirror TS `remoteCSeqIsNull === true ? null : remoteCSeq ?? null`:
            // the null bit wins; otherwise the value (absent ⇒ None).
            remote_cseq: if ext.remote_c_seq_is_null == Some(true) {
                None
            } else {
                ext.remote_c_seq.map(|n| n as i64)
            },
            inbound_pending_requests: ext
                .inbound_pending_requests
                .into_iter()
                .map(decode_pending_request)
                .collect::<Result<_, _>>()?,
            ack_branch: ext.ack_branch,
            pending_invite_txn: ext
                .pending_invite_txn_json
                .as_deref()
                .map(|s| from_json_string::<InviteTxnHandle>("Dialog.ext.pendingInviteTxn", s))
                .transpose()?,
            cached_sdp: ext.cached_sdp,
        },
    })
}

fn encode_pending_request(p: &PendingRequest) -> wire::PendingRequest {
    wire::PendingRequest {
        method: p.method.clone(),
        outbound_c_seq: p.outbound_cseq as i32,
        inbound_c_seq: p.inbound_cseq as i32,
        source_vias: p.source_vias.clone(),
        source_call_id: p.source_call_id.clone(),
        source_from: p.source_from.clone(),
        source_to: p.source_to.clone(),
        direction: direction_str(p.direction).into(),
    }
}
fn decode_pending_request(p: wire::PendingRequest) -> Result<PendingRequest, CallDecodeError> {
    Ok(PendingRequest {
        method: p.method,
        outbound_cseq: p.outbound_c_seq as i64,
        inbound_cseq: p.inbound_c_seq as i64,
        source_vias: p.source_vias,
        source_call_id: p.source_call_id,
        source_from: p.source_from,
        source_to: p.source_to,
        direction: direction_from(&p.direction)?,
    })
}

// ── Leg ─────────────────────────────────────────────────────────────────────

fn encode_leg(l: &Leg) -> wire::Leg {
    wire::Leg {
        leg_id: l.leg_id.clone(),
        call_id: l.call_id.clone(),
        from_tag: l.from_tag.clone(),
        source: Some(wire::RemoteInfo {
            address: l.source.address.clone(),
            port: l.source.port as i32,
        }),
        state: leg_state_str(l.state).into(),
        disposition: leg_disposition_str(l.disposition).into(),
        dialogs: l.dialogs.iter().map(encode_dialog).collect(),
        no_answer_timeout_sec: l.no_answer_timeout_sec.map(|n| n as f64),
        bye_disposition: l.bye_disposition.map(|b| bye_disposition_str(b).into()),
        local_uri: l.local_uri.clone(),
        remote_uri: l.remote_uri.clone(),
        invite_request_uri: l.invite_request_uri.clone(),
        pending_invite_txn_json: l.pending_invite_txn.as_ref().map(to_json_string),
        ext_json: l.ext.as_ref().map(to_json_string),
        kind: l.kind.map(|k| leg_kind_str(k).into()),
        adopted: l.adopted,
    }
}

fn decode_leg(l: wire::Leg) -> Result<Leg, CallDecodeError> {
    let source = l.source.ok_or_else(|| missing("Leg.source"))?;
    Ok(Leg {
        leg_id: l.leg_id,
        call_id: l.call_id,
        from_tag: l.from_tag,
        source: RemoteInfo {
            address: source.address,
            port: source.port as u16,
        },
        state: leg_state_from(&l.state)?,
        disposition: leg_disposition_from(&l.disposition)?,
        dialogs: l
            .dialogs
            .into_iter()
            .map(decode_dialog)
            .collect::<Result<_, _>>()?,
        no_answer_timeout_sec: l.no_answer_timeout_sec.map(|n| n as i64),
        bye_disposition: l.bye_disposition.as_deref().map(bye_disposition_from).transpose()?,
        local_uri: l.local_uri,
        remote_uri: l.remote_uri,
        invite_request_uri: l.invite_request_uri,
        pending_invite_txn: l
            .pending_invite_txn_json
            .as_deref()
            .map(|s| from_json_string::<InviteTxnHandle>("Leg.pendingInviteTxn", s))
            .transpose()?,
        ext: l
            .ext_json
            .as_deref()
            .map(|s| from_json_string::<ExtMap>("Leg.ext", s))
            .transpose()?,
        kind: l.kind.as_deref().map(leg_kind_from).transpose()?,
        adopted: l.adopted,
    })
}

// ── Top-level Call ──────────────────────────────────────────────────────────

/// `Call` (model) → `wire::Call`. Total: every distinction the model carries has
/// a wire home (a typed field or a `*IsNull` / `*Present` / `*Json` carry).
pub(crate) fn to_proto(call: &Call) -> wire::Call {
    wire::Call {
        call_ref: call.call_ref.clone(),
        a_leg: Some(encode_leg(&call.a_leg)),
        b_legs: call.b_legs.iter().map(encode_leg).collect(),

        // `Option<ActivePeer>`: `None` is the source's `null` (always-set bit).
        active_peer: call.active_peer.as_ref().map(|p| wire::ActivePeer {
            leg_a: p.leg_a.clone(),
            leg_b: p.leg_b.clone(),
        }),
        active_peer_is_null: call.active_peer.is_none(),

        callback_context: call.callback_context.clone(),
        // `billingContext` collapses absent/null in the Rust model (`Option`):
        // map `Some` to the value field; leave the null bit unset (it has no
        // Rust state to drive — decode ignores it). See the module header.
        billing_context: call.billing_context.clone(),
        billing_context_is_null: None,

        a_leg_invite: Some(encode_aleg_invite(&call.a_leg_invite)),
        limiter_entries: call.limiter_entries.iter().map(encode_limiter).collect(),
        timers: call.timers.iter().map(encode_timer).collect(),
        cdr_events: call.cdr_events.iter().map(encode_cdr).collect(),
        state: call_state_str(call.state).into(),
        created_at: call.created_at as f64,

        // `Option<Vec<String>>` — the present bit distinguishes `Some([])` from
        // absent (proto3 would otherwise drop an empty repeated field).
        a_leg_pending_vias: call.a_leg_pending_vias.clone().unwrap_or_default(),
        a_leg_pending_vias_present: call.a_leg_pending_vias.is_some(),

        a_leg_pending_c_seq: call.a_leg_pending_cseq.map(|n| n as i32),
        tag_map: call.tag_map.iter().map(encode_tagmap).collect(),
        trace_id: call.trace_id.clone(),
        root_span_id: call.root_span_id.clone(),
        sampled: call.sampled,
        worker_index: call.worker_index.map(|n| n as i32),
        topology: call.topology.as_ref().map(|t| wire::CallTopology {
            pri: t.pri.clone(),
            bak: t.bak.clone(),
            r#gen: t.gen as i32,
        }),
        emergency: call.emergency,

        features_json: call.features.as_ref().map(to_json_string),
        policy_update_headers_json: call.policy_update_headers.as_ref().map(to_json_string),
        // `Option<PolicyUpdateBody>` → the body bytes + null bit:
        //   None              → neither set (absent)
        //   Some(Empty)       → the source's `null` (force empty body) → null bit
        //   Some(Bytes(b))    → the value field
        policy_update_body: match &call.policy_update_body {
            Some(PolicyUpdateBody::Bytes(b)) => Some(b.clone()),
            _ => None,
        },
        policy_update_body_is_null: match &call.policy_update_body {
            Some(PolicyUpdateBody::Empty) => Some(true),
            _ => None,
        },

        active_rules: call
            .active_rules
            .as_ref()
            .map(|rs| rs.iter().map(encode_active_rule).collect())
            .unwrap_or_default(),
        active_rules_present: call.active_rules.is_some(),

        // `ruleState` is schema-reserved but the codec never populates it (no
        // `rule_state` on the Rust `Call`, matching the TS mapper). Default.
        rule_state: Vec::new(),
        rule_state_present: false,

        message_count: call.message_count.map(|n| n as i32),
        terminating_refresh_legs: call.terminating_refresh_legs.clone().unwrap_or_default(),
        terminating_refresh_legs_present: call.terminating_refresh_legs.is_some(),

        ext_json: call.ext.as_ref().map(to_json_string),
    }
}

/// `wire::Call` → `Call` (model). Fallible: a structurally-required submessage
/// missing, a bad enum token, or a malformed JSON carry is a corrupt body.
///
/// ## Carries the proto schema cannot represent (decode to default)
///
/// The `proto/call.proto` schema mirrors the **TS** `Call` shape, which is older
/// than two Rust-only model additions, so these fields have no wire home and come
/// back at their `None`/empty/`0` default:
///
/// - `relay_first_18x` / `promote_pem` / `transfer` / `sm_cursors` — the
///   ADR-0016 typed-ext slices. The TS `fromProtoObject` has the *same* scope:
///   it reconstructs `transfer` from the opaque `ext` carry, never a dedicated
///   proto field. (`ext` itself *does* round-trip, via `extJson`.)
/// - **`CallTopology.bak_gen` — the `(p,b)` version-vector backup counter
///   (ADR-0014).** The TS `CallTopology` is `{pri, bak, gen}` (single-counter
///   "newest gen wins" LWW); the proto faithfully mirrors it, so it has no
///   `bakGen` field and this codec resets `b` to `0` on every decode. The
///   production HA-replication codec is msgpack (which carries `b` via
///   `#[serde(default)]`); were protobuf ever chosen for replication, the causal
///   merge in `b2bua::repl::puller` would lose `b` across the hop. Surfaced here
///   because it is the one carry loss with a *correctness* (not just fidelity)
///   consequence — see the suite's `normalize_for_proto`.
///
/// All of this is acceptable for the codec's stated use because a codec swap is a
/// fresh-cluster event (ADR-0011) and msgpack is the replication codec — no body
/// mixes the two carriers. If protobuf is ever promoted to the replication path,
/// `call.proto` must first gain `bakGen` (+ the typed-ext fields).
pub(crate) fn from_proto(p: wire::Call) -> Result<Call, CallDecodeError> {
    let a_leg = decode_leg(p.a_leg.ok_or_else(|| missing("Call.aLeg"))?)?;
    let a_leg_invite = decode_aleg_invite(p.a_leg_invite.ok_or_else(|| missing("Call.aLegInvite"))?);

    Ok(Call {
        call_ref: p.call_ref,
        a_leg,
        b_legs: p
            .b_legs
            .into_iter()
            .map(decode_leg)
            .collect::<Result<_, _>>()?,
        active_peer: if p.active_peer_is_null {
            None
        } else {
            p.active_peer.map(|ap| ActivePeer {
                leg_a: ap.leg_a,
                leg_b: ap.leg_b,
            })
        },
        callback_context: p.callback_context,
        billing_context: p.billing_context,
        a_leg_invite,
        limiter_entries: p
            .limiter_entries
            .into_iter()
            .map(decode_limiter)
            .collect(),
        timers: p
            .timers
            .into_iter()
            .map(decode_timer)
            .collect::<Result<_, _>>()?,
        cdr_events: p
            .cdr_events
            .into_iter()
            .map(decode_cdr)
            .collect::<Result<_, _>>()?,
        state: call_state_from(&p.state)?,
        created_at: p.created_at as i64,
        a_leg_pending_vias: p
            .a_leg_pending_vias_present
            .then_some(p.a_leg_pending_vias),
        a_leg_pending_cseq: p.a_leg_pending_c_seq.map(|n| n as i64),
        tag_map: p.tag_map.into_iter().map(decode_tagmap).collect(),
        trace_id: p.trace_id,
        root_span_id: p.root_span_id,
        sampled: p.sampled,
        worker_index: p.worker_index.map(|n| n as i64),
        topology: p.topology.map(|t| crate::model::CallTopology {
            pri: t.pri,
            bak: t.bak,
            gen: t.r#gen as i64,
            bak_gen: 0,
        }),
        emergency: p.emergency,
        features: p
            .features_json
            .as_deref()
            .map(|s| from_json_string("Call.features", s))
            .transpose()?,
        policy_update_headers: p
            .policy_update_headers_json
            .as_deref()
            .map(|s| from_json_string("Call.policyUpdateHeaders", s))
            .transpose()?,
        // Reverse of the encode arm: null bit ⇒ `Some(Empty)`; else bytes ⇒
        // `Some(Bytes)`; neither ⇒ `None`.
        policy_update_body: if p.policy_update_body_is_null == Some(true) {
            Some(PolicyUpdateBody::Empty)
        } else {
            p.policy_update_body.map(PolicyUpdateBody::Bytes)
        },
        active_rules: p
            .active_rules_present
            .then(|| p.active_rules.into_iter().map(decode_active_rule).collect()),
        ext: p
            .ext_json
            .as_deref()
            .map(|s| from_json_string::<ExtMap>("Call.ext", s))
            .transpose()?,
        message_count: p.message_count.map(|n| n as i64),
        terminating_refresh_legs: p
            .terminating_refresh_legs_present
            .then_some(p.terminating_refresh_legs),
        // No proto field — defaults (see the doc note above).
        relay_first_18x: None,
        promote_pem: None,
        transfer: None,
        sm_cursors: Default::default(),
    })
}

// ── Leaf encoders / decoders ────────────────────────────────────────────────

fn encode_aleg_invite(a: &ALegInviteSnapshot) -> wire::ALegInvite {
    wire::ALegInvite {
        uri: a.uri.clone(),
        headers: a
            .headers
            .iter()
            .map(|h| wire::SipHeader {
                name: h.name.clone(),
                value: h.value.clone(),
            })
            .collect(),
        body: a.body.clone(),
    }
}
fn decode_aleg_invite(a: wire::ALegInvite) -> ALegInviteSnapshot {
    ALegInviteSnapshot {
        uri: a.uri,
        headers: a
            .headers
            .into_iter()
            .map(|h| SipHeader {
                name: h.name,
                value: h.value,
            })
            .collect(),
        body: a.body,
    }
}

fn encode_limiter(e: &CallLimiterState) -> wire::CallLimiterState {
    wire::CallLimiterState {
        limiter_id: e.limiter_id.clone(),
        limit: e.limit as i32,
        origin_window: e.origin_window as f64,
        increment_succeeded: e.increment_succeeded,
    }
}
fn decode_limiter(e: wire::CallLimiterState) -> CallLimiterState {
    CallLimiterState {
        limiter_id: e.limiter_id,
        limit: e.limit as i64,
        origin_window: e.origin_window as i64,
        increment_succeeded: e.increment_succeeded,
    }
}

fn encode_timer(t: &TimerEntry) -> wire::TimerEntry {
    wire::TimerEntry {
        id: t.id.clone(),
        r#type: timer_type_str(t.timer_type).into(),
        fire_at: t.fire_at as f64,
        leg_id: t.leg_id.clone(),
    }
}
fn decode_timer(t: wire::TimerEntry) -> Result<TimerEntry, CallDecodeError> {
    Ok(TimerEntry {
        id: t.id,
        timer_type: timer_type_from(&t.r#type)?,
        fire_at: t.fire_at as i64,
        leg_id: t.leg_id,
    })
}

fn encode_cdr(c: &CdrEvent) -> wire::CdrEvent {
    wire::CdrEvent {
        r#type: cdr_type_str(c.event_type).into(),
        timestamp: c.timestamp as f64,
        leg_id: c.leg_id.clone(),
        status_code: c.status_code.map(|n| n as i32),
        reason: c.reason.clone(),
    }
}
fn decode_cdr(c: wire::CdrEvent) -> Result<CdrEvent, CallDecodeError> {
    Ok(CdrEvent {
        event_type: cdr_type_from(&c.r#type)?,
        timestamp: c.timestamp as i64,
        leg_id: c.leg_id,
        status_code: c.status_code.map(|n| n as i64),
        reason: c.reason,
    })
}

fn encode_tagmap(m: &TagMapping) -> wire::TagMapping {
    wire::TagMapping {
        a_tag: m.a_tag.clone(),
        b_leg_id: m.b_leg_id.clone(),
        b_tag: m.b_tag.clone(),
    }
}
fn decode_tagmap(m: wire::TagMapping) -> TagMapping {
    TagMapping {
        a_tag: m.a_tag,
        b_leg_id: m.b_leg_id,
        b_tag: m.b_tag,
    }
}

fn encode_active_rule(r: &ActiveRule) -> wire::ActiveRule {
    // The production `ActiveRule` is `{id, active}` (`CallModel.ts` + the Rust
    // model). `params{Present,Json}` are schema-reserved and never written —
    // matching `protobuf.ts`'s `encodeActiveRule`. Defaults keep the tags free.
    wire::ActiveRule {
        id: r.id.clone(),
        params_present: false,
        params_json: None,
        active: r.active,
    }
}
fn decode_active_rule(r: wire::ActiveRule) -> ActiveRule {
    ActiveRule {
        id: r.id,
        active: r.active,
    }
}

fn missing(field: &str) -> CallDecodeError {
    CallDecodeError::Decode(format!("required field {field} missing from proto body"))
}
