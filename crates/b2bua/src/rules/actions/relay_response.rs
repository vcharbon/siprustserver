//! Relaying an inbound SIP **response** toward the peer (normally the a-leg):
//! pending-correlated transparent relay, the per-fork a-facing tag map on
//! b-leg INVITE 1xx/2xx, the a-leg server-txn regeneration default, and the
//! bare-180 downgrade (`relayFirst18xTo180`). Request relay does NOT live
//! here — see [`super::relay_request`].

use call::helpers::{add_tag_mapping, find_by_b_tag, remove_pending_request, find_pending_request};
use call::{Call, TagMapping, TimerType};
use sip_message::draft::Entry;
use sip_message::generators::{self, GenerateRelayedResponseOpts};
use sip_message::header::{HeaderName, HeaderValue, MediaType, Via};
use sip_message::{SipHeader, SipStr};

use crate::effects::{HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::rules::capabilities::{self, Face};
use crate::rules::model::{MessageTransform, RuleContext};
use crate::rules::relay;

use super::select::{dialog_identity_tag, resolve_peer};
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Relay an inbound SIP response toward `target_leg` (normally the a-leg).
    /// Two paths, mirroring the source:
    ///   - **pending-correlated** (in-dialog non-INVITE: PRACK/OPTIONS/INFO/
    ///     UPDATE/…): rebuild from the snapshot captured when the request was
    ///     relayed, so the response echoes the caller's Via/From/To/CSeq.
    ///   - **default** (initial-INVITE 1xx/2xx): regenerate on the a-leg server
    ///     transaction, establishing the b-leg early dialog + a-facing tag map
    ///     on a reliable 1xx so a later PRACK can target the callee.
    pub(super) fn relay_response(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
        resp: &sip_message::SipResponse,
    ) {
        let status = transform.status.unwrap_or(resp.status());
        let reason = transform.reason.clone().unwrap_or_else(|| resp.reason().to_string());
        // The body relayed toward alice: dropped (bare-180 downgrade), replaced
        // by a staged policy body (fake-prack cached SDP on the 200 OK), or the
        // response's own body verbatim.
        let (relay_body, relay_content_type): (Vec<u8>, Option<MediaType>) = if transform.drop_body
        {
            (vec![], None)
        } else if let Some(call::PolicyUpdateBody::Bytes(b)) = call.policy_update_body.clone() {
            (b, Some(relay::sdp()))
        } else {
            (
                resp.body().to_vec(),
                resp.raw(HeaderName::ContentType).next().and_then(relay::media_type),
            )
        };
        // Passthrough headers minus any the transform suppresses (e.g.
        // Require/RSeq on a bare-180 downgrade), plus any the transform stamps
        // with replace semantics (Allow/Supported on the synthetic 200 / resync
        // re-INVITE, `promote18xPemTo200`).
        let add_headers = transform.add_headers.clone();
        let filter_passthrough = move |hs: Vec<SipHeader>| -> Vec<SipHeader> {
            let mut out: Vec<SipHeader> = hs
                .into_iter()
                .filter(|h| {
                    !transform.remove_headers.iter().any(|r| r.matches(&h.name))
                        && !add_headers.iter().any(|e| e.name().matches(&h.name))
                })
                .collect();
            for entry in &add_headers {
                out.push(SipHeader {
                    name: SipStr::owned(entry.name().as_wire_str()),
                    value: entry.text(),
                });
            }
            out
        };
        let cseq = resp.cseq();
        let cseq_num = cseq.seq() as i64;
        let cseq_method = if cseq.method().as_str().is_empty() {
            "INVITE".to_string()
        } else {
            cseq.method().to_string()
        };
        let to_tag = resp.to().tag().unwrap_or_default().to_string();
        let source_leg_id = ctx.source_leg_id.to_string();

        // ── Pending transparent-relay correlation (§8.1.3.3) ──
        // Resolve the *exact* source dialog the response belongs to by its
        // To-tag (the responder's tag = this leg's dialog `remote_tag`). Under
        // forking (RFC 3261 §12.1.2) the source leg holds several early dialogs;
        // `source_dialog()` would return `dialogs.first()` (fork 1), so fork 2's
        // PRACK/UPDATE response would miss its pending entry and fall through to
        // the INVITE-response regeneration below — corrupting a `200 (PRACK)` /
        // `200 (UPDATE)` into a spurious `200 (INVITE)` toward the caller.
        let src_dialog = ctx
            .source_leg()
            .and_then(|leg| call::helpers::find_dialog_by_to_tag(leg, &to_tag))
            .or_else(|| ctx.source_dialog())
            .cloned();
        if let Some(src_dialog) = src_dialog {
            if let Some(pending) = find_pending_request(&src_dialog, cseq_num).cloned() {
                // §18.2.2: a relayed response goes to the originator's top Via
                // sent-by, and the snapshot holds that Via verbatim. A Via no
                // reader accepts names no address — drop the relay and say so
                // (the originator retransmits, then times out its own request).
                // Answering it toward a fabricated destination would post the
                // caller's response to whatever that address happens to be; the
                // loopback this used to fall back to swallowed it silently (055).
                let Some(dest) = pending.source_vias.first().and_then(|v| via_sent_by(v)) else {
                    eprintln!(
                        "WARN: call {}: leg {source_leg_id}: relayed {status} dropped — the \
                         originator's top Via does not read, so it names no destination",
                        call.call_ref
                    );
                    return;
                };
                let contact = relay::leg_contact(self.config, &call.call_ref, target_leg, call.emergency == Some(true));
                let mut transparent_headers =
                    filter_passthrough(relay::relay_response_passthrough_headers(resp));
                // A 2xx answer to a B2BUA-relayed re-INVITE advertises the B2BUA's
                // own Allow/Supported toward the peer (RFC 3261 §13.2.1/§20.37),
                // replacing the source response's. Non-INVITE 2xx (PRACK/UPDATE)
                // and provisionals keep verbatim passthrough.
                if cseq_method == "INVITE" && (200..300).contains(&status) {
                    let caps = capabilities::for_leg(call, target_leg);
                    relay::stamp_a_facing_invite_advert(&mut transparent_headers, &transform.add_headers, &caps);
                }
                // §8.2.6.2 makes Via / From / To / Call-ID / CSeq equal the
                // originator's, and the snapshot holds the originator's own
                // bytes (the `call` crate stores text, ADR-0008) — so each
                // rides as a raw entry and reaches the wire unaltered.
                let echo = |name: HeaderName, text: &str| Entry::raw(name, SipStr::owned(text));
                let opts = GenerateRelayedResponseOpts {
                    vias: pending
                        .source_vias
                        .iter()
                        .map(|v| echo(HeaderName::Via, v))
                        .collect(),
                    record_routes: vec![],
                    from: Some(echo(HeaderName::From, &pending.source_from)),
                    to: Some(echo(HeaderName::To, &pending.source_to)),
                    call_id: Some(echo(HeaderName::CallId, &pending.source_call_id)),
                    cseq: Some(echo(
                        HeaderName::CSeq,
                        &format!("{} {}", pending.inbound_cseq, cseq_method),
                    )),
                    body: relay_body.clone(),
                    transparent_headers,
                    content_type: relay_content_type.clone(),
                    contact: Some(contact),
                };
                let relayed = generators::generate_relayed_response(status, &reason, &opts);
                let s_id = dialog_identity_tag(&source_leg_id, &src_dialog);
                // §8.1.3.3 / §14.1: the snapshot correlates EVERY response of the
                // transaction back to the originator, so it must outlive a relayed
                // provisional (1xx) — a re-INVITE may send 18x THEN a non-2xx final,
                // and dropping the snapshot on the 18x would orphan the final
                // (it would miss `relay-reinvite-response`, fall through to
                // `route-failure`, and wrongly tear the call down). Remove it only
                // on the FINAL (>= 200); the transaction has no later response then.
                if status >= 200 {
                    *call = remove_pending_request(call.clone(), &source_leg_id, &s_id, cseq_num);
                }
                // RFC 3261 §13.3.1.4 (in-dialog): a **2xx to a re-INVITE the
                // originator (a-leg) issued** was relayed via the a-leg server
                // txn, which goes `Completed` on this final and will NOT
                // retransmit it — so a lost a-leg ACK strands the renegotiation.
                // Cache the exact outbound bytes + arm the un-ACKed-re-INVITE-2xx
                // watchdog (the initial-INVITE `AckRetransmit`/`AckTimeout` twin).
                // Only a genuine originator re-INVITE reaches here: the initial
                // 2xx carries no relay snapshot (it takes the per-fork branch
                // below), and a B2BUA-originated re-INVITE (realign / reroute /
                // promote, whose 2xx comes back FromA and is ACKed by the B2BUA)
                // leaves no snapshot either — so a reclaim/realign leg never arms
                // this. `ack_timeout_sec <= 0` disables the watchdog (as initial).
                if cseq_method == "INVITE"
                    && (200..300).contains(&status)
                    && target_leg == call.a_leg.leg_id
                    && self.config.ack_timeout_sec > 0
                {
                    if let Some(d) = call.a_leg.dialogs.first_mut() {
                        d.ext.pending_reinvite_2xx = Some(call::PendingReinvite2xx {
                            response: relayed.image().to_vec(),
                            dest_host: dest.0.clone(),
                            dest_port: dest.1,
                            // The relayed 2xx echoes the originator's re-INVITE
                            // CSeq (`pending.inbound_cseq`); the a-leg ACK carries
                            // it, so only that ACK quiesces the watchdog.
                            cseq: pending.inbound_cseq,
                        });
                    }
                    self.schedule(
                        call,
                        fx,
                        TimerType::ReinviteAckRetransmit,
                        crate::rules::defaults::ACK_RETRANSMIT_SEC * 1000,
                        None,
                    );
                    self.schedule(
                        call,
                        fx,
                        TimerType::ReinviteAckTimeout,
                        self.config.ack_timeout_sec * 1000,
                        None,
                    );
                }
                fx.outbound.push(OutboundSipEffect {
                    body: OutboundBody::Response(relayed),
                    mode: OutboundTxnMode::ServerResponse,
                    destination: dest,
                    label: format!("{status} {cseq_method} → {target_leg}"),
                    leg_id: Some(target_leg.to_string()),
                });
                return;
            }
        }

        // ── b-leg INVITE 1xx/2xx → per-fork a-facing tag map ──
        // Each callee early dialog (forking → several per b-leg) gets its own
        // a-facing tag so the caller sees independent early dialogs; the response
        // is relayed under that per-fork tag (RFC 3261 §12; source confirm/relay).
        // The 2xx is included so that when a *non-first* fork wins, the confirmed
        // dialog the caller sees carries the WINNING fork's a-tag — not the first
        // fork's primary (RFC 3261 §13.2.2.4) — so the caller's ACK/in-dialog
        // requests address the dialog the B2BUA actually established.
        if cseq_method == "INVITE"
            && (100..300).contains(&resp.status())
            && !to_tag.is_empty()
            && source_leg_id != "a"
        {
            self.track_b_early_dialog(call, &source_leg_id, resp, &to_tag);
            let a_face = match find_by_b_tag(call, &source_leg_id, &to_tag) {
                Some(m) => m.a_tag.clone(),
                None => {
                    // First fork on this leg reuses the leg's primary a-tag (keeps
                    // a single confirmed a-dialog stable); later forks mint fresh.
                    let primary = self.ensure_a_dialog(call);
                    let a_face = if call.tag_map.iter().any(|m| m.b_leg_id == source_leg_id) {
                        self.id_gen.new_tag()
                    } else {
                        primary
                    };
                    *call = add_tag_mapping(
                        call.clone(),
                        TagMapping {
                            a_tag: a_face.clone(),
                            b_leg_id: source_leg_id.clone(),
                            b_tag: to_tag.to_string(),
                        },
                    );
                    a_face
                }
            };
            let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
            let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
            let mut passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
            // A 2xx INVITE answer the B2BUA mints toward the caller advertises the
            // B2BUA's own capability set (RFC 3261 §13.2.1/§20.37), replacing any
            // Allow/Supported the callee's 200 carried. Provisionals keep verbatim
            // passthrough so reliable-1xx (Supported:100rel) negotiation survives.
            if (200..300).contains(&status) {
                let caps = capabilities::advertised(call, Face::Originator);
                relay::stamp_a_facing_invite_advert(&mut passthrough, &transform.add_headers, &caps);
            }
            let effect = relay::response_to_a_leg(
                &a_invite,
                status,
                &reason,
                Some(a_face),
                Some(contact),
                relay_body,
                relay_content_type,
                None,
                passthrough,
            );
            fx.outbound.push(effect);
            return;
        }

        // A non-INVITE response that reached here failed pending correlation
        // (§8.1.3.3): there is no a-leg transaction to answer. Regenerating it as
        // an INVITE response would emit a spurious `200 (INVITE)` toward the
        // caller (the exact forking corruption above). Drop it instead — only an
        // INVITE response legitimately regenerates on the a-leg server txn.
        if cseq_method != "INVITE" {
            return;
        }

        // ── Default: regenerate the INVITE response on the a-leg server txn ──
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        // Reliable-provisional negotiation headers (Require/Supported/RSeq) pass
        // through transparently so end-to-end PRACK keeps working (RFC 3262).
        let mut passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
        // A 2xx INVITE answer carries the B2BUA's own Allow/Supported, replacing
        // the callee's (RFC 3261 §13.2.1/§20.37); provisionals keep passthrough.
        if (200..300).contains(&status) {
            let caps = capabilities::advertised(call, Face::Originator);
            relay::stamp_a_facing_invite_advert(&mut passthrough, &transform.add_headers, &caps);
        }
        let effect = relay::response_to_a_leg(
            &a_invite,
            status,
            &reason,
            Some(a_tag),
            Some(contact),
            relay_body,
            relay_content_type,
            None,
            passthrough,
        );
        fx.outbound.push(effect);
    }

    /// Bare-180 downgrade relay ([`crate::rules::model::RuleAction::RelayFirstBare180`]).
    /// Mint the a-facing To-tag on the FIRST 18x (the executor owns the IdGen); a
    /// LATER 18x the `relay18x.messages` policy relays again (ALL / ONE_PER_VALUE)
    /// reuses the stored tag — the masking property presents ONE stable early
    /// dialog to the caller regardless of which fork rings. Seed the tag map for
    /// this b-leg dialog, record it (+ the upstream status value for
    /// ONE_PER_VALUE dedupe), then relay the current 1xx as a bare 180. The relay
    /// path resolves the a-facing tag from the map (`find_by_b_tag`).
    pub(super) fn relay_first_bare_180(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        b_tag: &str,
    ) {
        let a_facing_tag = call::helpers::relay_first_18x_stored_a_tag(call)
            .map(str::to_string)
            .unwrap_or_else(|| self.id_gen.new_tag());
        *call = add_tag_mapping(
            call.clone(),
            TagMapping {
                a_tag: a_facing_tag.clone(),
                b_leg_id: leg_id.to_string(),
                b_tag: b_tag.to_string(),
            },
        );
        *call = call::helpers::set_relay_first_18x_relayed(call.clone(), &a_facing_tag);
        if let Some(resp) = ctx.response() {
            *call = call::helpers::record_relay_first_18x_value(call.clone(), resp.status());
        }
        let transform = MessageTransform {
            status: Some(180),
            reason: Some("Ringing".to_string()),
            drop_body: true,
            remove_headers: vec![HeaderName::Require, HeaderName::RSeq],
            add_headers: vec![],
        };
        let (peer, target_to_tag) = resolve_peer(call, ctx);
        if let Some(peer) = peer {
            self.relay_to(call, fx, ctx, &peer, &transform, target_to_tag);
        }
    }
}

/// The sent-by a response to this hop is routed to (RFC 3261 §18.2.2). The
/// snapshot the pending-request correlation holds is text (the `call` crate has
/// no sip-message dependency), so reading one back is a parse.
fn via_sent_by(via: &str) -> Option<(String, u16)> {
    let hop = Via::parse(&SipStr::owned(via)).ok()?;
    let (host, port) = hop.sent_by().pair();
    Some((host.to_string(), port))
}
