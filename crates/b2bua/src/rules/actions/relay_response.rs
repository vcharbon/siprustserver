//! Relaying an inbound SIP **response** toward the peer (normally the a-leg):
//! pending-correlated transparent relay, the per-fork a-facing tag map on
//! b-leg INVITE 1xx/2xx, the a-leg server-txn regeneration default, and the
//! bare-180 downgrade (`relayFirst18xTo180`). Request relay does NOT live
//! here — see [`super::relay_request`].

use call::helpers::{
    add_tag_mapping, find_by_b_tag, find_pending_request, remove_pending_request, Scope,
};
use call::{Call, LegState, PendingRequest, TagMapping};
use sip_message::draft::Entry;
use sip_message::generators::{self, GenerateRelayedResponseOpts, SourceBody};
use sip_message::header::{HeaderName, HeaderValue, MediaType, To, Via};
use sip_message::{Method, SipHeader, SipStr};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities::{self, Face};
use crate::rules::model::{MessageTransform, RuleContext};
use crate::rules::relay;

use super::select::{dialog_identity_tag, resolve_peer};
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Restate a relayed reliable provisional's `RSeq` with the shown dialog's
    /// own number (RFC 3262 §4, errata 4603) and remember what it stands for,
    /// so the PRACK naming it translates back onto the responder's sequence.
    /// `shown_tag` is this stack's tag on the dialog the provisional is
    /// relayed into and `shown_invite_cseq` the CSeq of the INVITE it answers
    /// on that face — the two the PRACK's `RAck` names beside the number
    /// (§7.2). A retransmitted provisional recalls the number already shown; a
    /// provisional relayed without an `RSeq` (a masking policy stripped it) is
    /// not a reliable one and takes no number. Returns the number taken, so the
    /// emission arms its §3 ladder.
    fn own_relayed_rseq(
        &self,
        call: &mut Call,
        shown_tag: &str,
        shown_invite_cseq: i64,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        headers: &mut [SipHeader],
    ) -> Option<i64> {
        if !headers.iter().any(|h| HeaderName::RSeq.matches(&h.name)) {
            return None;
        }
        let b_rseq = relay::reliable_rseq(resp)?;
        let b_tag = resp.to().tag().unwrap_or_default().to_string();
        let initial = if call::helpers::starts_reliable_ladder(call, shown_tag) {
            self.id_gen.new_sequence_number() as i64
        } else {
            0
        };
        let b_cseq = i64::from(resp.cseq().seq());
        let (updated, a_rseq) = call::helpers::assign_a_rseq(
            call.clone(),
            shown_tag,
            shown_invite_cseq,
            source_leg_id,
            &b_tag,
            b_cseq,
            b_rseq,
            initial,
        );
        *call = updated;
        relay::own_the_rseq(headers, a_rseq);
        Some(a_rseq)
    }

    /// Relay an inbound SIP response toward `target_leg` (normally the a-leg).
    /// Two paths, mirroring the source:
    ///   - **pending-correlated** (any relayed in-dialog request: re-INVITE/
    ///     PRACK/OPTIONS/INFO/UPDATE/…): rebuild from the snapshot captured
    ///     when the request was relayed, so the response echoes the
    ///     originator's Via/From/To/CSeq; a reliable provisional leaves under
    ///     this stack's own `RSeq` where RFC 3262 admits one at all.
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
        // The staged body is ONE-SHOT: armed per 2xx by the staging rule and taken
        // here, so it never substitutes itself into a later relayed response.
        // `ConfirmDialog` runs before this relay, so the retransmit copy it
        // stashes still sees it.
        // `relay_source_body` — what the relayed message carries where THIS
        // response had a body, which decides how much of the set describing it
        // travels (§16.6): a staged body of the same role keeps the header
        // stating that role, a dropped body keeps none.
        let (relay_body, relay_content_type, relay_source_body): (
            Vec<u8>,
            Option<MediaType>,
            SourceBody,
        ) = if transform.drop_body {
            (vec![], None, SourceBody::Dropped)
        } else if let Some(call::PolicyUpdateBody::Bytes(b)) = call.policy_update_body.take() {
            (b, Some(relay::sdp()), SourceBody::Replaced)
        } else {
            (
                resp.body().to_vec(),
                resp.raw(HeaderName::ContentType).next().and_then(relay::media_type),
                SourceBody::Verbatim,
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

        // ── A repeat of a reliable provisional already handled is absorbed ──
        // A recorded entry — relayed under a shown number, or PRACKed by this
        // stack itself where the originator was shown it unreliably — proves
        // this datagram has been dealt with once, so a repeat is a
        // retransmission the UAC discards outright (RFC 3262 §4 —
        // unconditionally, PRACKed or not); it dies here, before
        // `own_relayed_rseq` and any emission, on either relay path. The far
        // party's own further copies are this stack's §3 ladder
        // (`arm_reliable_provisional_ladder`), never the responder's clock; a
        // responder's DISTINCT next provisional matches no entry and relays.
        if relay::repeated_reliable_provisional(call, &source_leg_id, resp) {
            return;
        }

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
                    tracing::warn!(
                        call_ref = %call.call_ref,
                        leg_id = %source_leg_id,
                        status,
                        "relayed response dropped — the originator's top Via does not read, so it \
                         names no destination"
                    );
                    return;
                };
                // The B2BUA's Contact rides only where it establishes a dialog
                // or answers a target refresh (`response_states_contact`) — a
                // relayed 200 to PRACK/OPTIONS/INFO/MESSAGE states none.
                let contact =
                    generators::response_states_contact(&Method::from_wire(&cseq_method), status)
                        .then(|| {
                            relay::leg_contact(
                                self.config,
                                &call.call_ref,
                                target_leg,
                                call.emergency == Some(true),
                            )
                        });
                let mut transparent_headers = filter_passthrough(
                    relay::relay_response_passthrough_headers(resp, relay_source_body),
                );
                // A 2xx answer to a B2BUA-relayed re-INVITE advertises this
                // face's capability set (RFC 3261 §13.2.1/§20.37) — the source
                // response's own, carried through, unless the call declares one.
                // Non-INVITE 2xx (PRACK/UPDATE) and provisionals keep verbatim
                // passthrough.
                if cseq_method == "INVITE" && (200..300).contains(&status) {
                    let caps =
                        capabilities::relaying_for_leg(call, target_leg, &transparent_headers);
                    relay::stamp_a_facing_invite_advert(
                        &mut transparent_headers,
                        &transform.add_headers,
                        &caps,
                    );
                }
                // A reliable provisional leaves toward the originator under
                // THIS stack's number — it is the UAS of that face, and RFC
                // 3262 §3 makes the sequence the sender's, per transaction —
                // recorded so the PRACK naming it translates back, and only
                // where §3 admits one at all (`admits_reliable_provisional`:
                // an INVITE whose originator offered `100rel`). Otherwise it
                // leaves as an ordinary provisional, and the responder's
                // reliable one is this stack's to acknowledge: it offered the
                // extension there, the originator did not. No rule relays a
                // non-INVITE provisional here (RFC 4320 §4.1 discards it);
                // the method guard on the PRACK states that, nothing more.
                let mut ladder: Option<(String, i64)> = None;
                if let Some(b_rseq) =
                    relay::reliable_rseq(resp).filter(|_| (101..200).contains(&status))
                {
                    let shown_tag = call::helpers::admits_reliable_provisional(&pending)
                        .then(|| shown_tag_of(call, target_leg, &pending))
                        .flatten();
                    match shown_tag {
                        Some(shown_tag) => {
                            ladder = self
                                .own_relayed_rseq(
                                    call,
                                    &shown_tag,
                                    pending.inbound_cseq,
                                    &source_leg_id,
                                    resp,
                                    &mut transparent_headers,
                                )
                                .map(|a_rseq| (shown_tag, a_rseq));
                        }
                        None => {
                            relay::strip_reliability(&mut transparent_headers);
                            if cseq_method == "INVITE" {
                                self.send_prack_to_leg(
                                    call,
                                    fx,
                                    &source_leg_id,
                                    b_rseq,
                                    cseq_num,
                                    &to_tag,
                                );
                            }
                        }
                    }
                }
                let opts = snapshot_response_opts(
                    &pending,
                    &cseq_method,
                    relay_body.clone(),
                    relay_content_type.clone(),
                    transparent_headers,
                    contact,
                );
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
                    // The final ends this transaction's reliable provisionals
                    // on the face they were shown: no rung follows it (RFC
                    // 3262 §3, RFC 3261 §17.2.1). This transaction's alone —
                    // a re-INVITE's final is not the setup's.
                    if cseq_method == "INVITE" {
                        self.retire(
                            call,
                            fx,
                            Scope::Transaction { leg_id: &source_leg_id, cseq: cseq_num },
                        );
                    }
                }
                // RFC 3261 §13.3.1.4 (in-dialog): a **2xx to a re-INVITE the
                // originator (either face) issued** was relayed via that face's
                // server txn, which goes `Completed` on this final and will NOT
                // retransmit it — so a lost originator ACK strands the
                // renegotiation. Retain the exact outbound bytes on the
                // originator's dialog + arm its ladder (`retain_reinvite_2xx`),
                // keyed by the CSeq the relayed 2xx echoes — the originator's
                // re-INVITE CSeq, which only its own ACK carries. Only a
                // genuine originator re-INVITE reaches here: the initial 2xx
                // carries no relay snapshot (it takes the per-fork branch
                // below), and a B2BUA-originated re-INVITE (realign / reroute /
                // promote, whose 2xx is ACKed by the B2BUA itself) leaves no
                // snapshot either — so a reclaim/realign leg never arms this.
                if cseq_method == "INVITE" && (200..300).contains(&status) {
                    // RFC 3261 §13.2.2.4 on the ANSWERING leg — `confirm_dialog`'s
                    // in-dialog twin: the ACK this 2xx owes is the originator's
                    // own, relayed end-to-end when it arrives, so arm the
                    // obligation for the CSeq it carries (the originator's own
                    // re-INVITE CSeq). Its emission mints the ACK's client
                    // transaction and retains the branch every later copy of the
                    // 2xx is re-ACKed on. The ladder below is discharged by
                    // RECEIVING that ACK (the engine matches it on the
                    // originator's dialog), never by relaying it.
                    *call = call::helpers::set_awaited_ack_cseq(
                        call.clone(),
                        &source_leg_id,
                        Some(pending.inbound_cseq),
                    );
                    self.retain_reinvite_2xx(call, fx, &relayed, dest.clone(), target_leg);
                }
                let effect = OutboundSipEffect {
                    body: OutboundBody::Response(relayed),
                    mode: OutboundTxnMode::ServerResponse,
                    destination: dest,
                    label: format!("{status} {cseq_method} → {target_leg}"),
                    leg_id: Some(target_leg.to_string()),
                    provenance: Provenance::Relayed,
                };
                // A reliable provisional leaving under our own number is ours
                // to repeat until PRACKed (RFC 3262 §3): retain it + arm the
                // ladder on the face it left on.
                if let Some((shown_tag, a_rseq)) = ladder {
                    self.arm_reliable_provisional_ladder(call, fx, &effect, &shown_tag, a_rseq);
                }
                fx.outbound.push(effect);
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
        // The range IS the split: the answering fork's tag on a 2xx, the leg's
        // primary on a non-2xx final — which ends the transaction and
        // establishes nothing, so it names no dialog the caller keeps.
        if cseq_method == "INVITE"
            && (100..300).contains(&resp.status())
            && !to_tag.is_empty()
            && source_leg_id != "a"
        {
            self.track_b_early_dialog(call, &source_leg_id, resp, &to_tag);
            let a_face = match find_by_b_tag(call, &source_leg_id, &to_tag) {
                Some(m) => m.a_tag.clone(),
                None => {
                    // Which caller-facing dialog a new callee early dialog lands
                    // in is the 18x policy's to decide (`sipProfile.relay18x` /
                    // `.prack`), and the two policies answer it oppositely.
                    //
                    // TRANSPARENT (no `relayFirst18xTo180` arm): the caller's
                    // dialog set mirrors the callee's, so every early dialog
                    // past the first this CALL published mints its own a-tag —
                    // a rerouted leg's included. That is what keeps a second
                    // reliable provisional out of an unacknowledged one's
                    // dialog: §3's ban is per early dialog (§4, errata
                    // 4603/4604), and `assign_a_rseq` seeds the fresh tag its
                    // own `RSeq` space.
                    //
                    // MASKING (`drop-sdp` / `keep-sdp` / `fake-prack` /
                    // `promote-pem-to-200`): the caller keeps ONE identity
                    // across forking and failover — `relay_first_18x`'s stated
                    // purpose — so only a second fork ON THIS LEG mints, and a
                    // rerouted leg fuses back onto the primary. §3 cannot be
                    // reached there: the caller is shown a bare 180, never a
                    // reliable provisional.
                    let primary = self.ensure_a_dialog(call);
                    let mirrors_callee_dialogs =
                        call::helpers::relay_first_18x_strategy(call).is_none();
                    // …and only while the caller's INVITE is still in SETUP.
                    // An early dialog exists only between a provisional and the
                    // final that ends it (RFC 3261 §12.1/§13.2.2.3), so once she
                    // is answered there is no early dialog left to mirror: a leg
                    // ringing after that point (an MRF hold, a transfer target)
                    // rings inside a dialog she has CONFIRMED, and a fresh tag
                    // there re-identifies an established dialog instead of
                    // opening a second early one.
                    let in_setup = call.a_leg.state != LegState::Confirmed;
                    let already_published = if mirrors_callee_dialogs && in_setup {
                        !call.tag_map.is_empty()
                    } else {
                        call.tag_map.iter().any(|m| m.b_leg_id == source_leg_id)
                    };
                    let a_face = if already_published { self.id_gen.new_tag() } else { primary };
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
            let contact = relay::leg_contact(
                self.config,
                &call.call_ref,
                &call.a_leg.leg_id,
                call.emergency == Some(true),
            );
            let mut passthrough = filter_passthrough(relay::relay_response_passthrough_headers(
                resp,
                relay_source_body,
            ));
            // A 2xx INVITE answer the B2BUA mints toward the caller advertises the
            // capability set of the originator face (RFC 3261 §13.2.1/§20.37):
            // the callee's own, carried through, unless the call declares one.
            // Provisionals keep verbatim passthrough so reliable-1xx
            // (Supported:100rel) negotiation survives.
            if (200..300).contains(&status) {
                let caps = capabilities::relaying(call, Face::Originator, &passthrough);
                relay::stamp_a_facing_invite_advert(
                    &mut passthrough,
                    &transform.add_headers,
                    &caps,
                );
            }
            let a_rseq = self.own_relayed_rseq(
                call,
                &a_face,
                i64::from(a_invite.cseq().seq()),
                &source_leg_id,
                resp,
                &mut passthrough,
            );
            let Some(effect) = relay::response_to_a_leg(
                call,
                fx,
                &a_invite,
                status,
                &reason,
                Some(a_face.clone()),
                Some(contact),
                relay_body,
                relay_content_type,
                None,
                passthrough,
                Provenance::Relayed,
            ) else {
                return;
            };
            // A 2xx answers the caller: it goes through the one seam that
            // retains the datagram + arms the §13.3.1.4 ladder.
            if (200..300).contains(&status) {
                self.send_a_leg_answer(call, fx, effect);
            } else {
                // A reliable provisional leaving under our own number is ours
                // to repeat until PRACKed (RFC 3262 §3): retain it + arm the
                // caller-facing ladder.
                if let Some(a_rseq) = a_rseq.filter(|_| (101..200).contains(&status)) {
                    self.arm_reliable_provisional_ladder(call, fx, &effect, &a_face, a_rseq);
                }
                fx.outbound.push(effect);
            }
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
        // The a-dialog's tag, whoever showed it first: a caller already put in an
        // early dialog is answered under that same tag (§8.2.6.2), including the
        // owned bare 180's under `relayFirst18xTo180`.
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(
            self.config,
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
        );
        // Reliable-provisional negotiation (Require/Supported) passes through
        // transparently so end-to-end PRACK keeps working (RFC 3262); the RSeq
        // it rides on is this transaction's own (`own_relayed_rseq`).
        let mut passthrough =
            filter_passthrough(relay::relay_response_passthrough_headers(resp, relay_source_body));
        // A 2xx INVITE answer carries the originator face's Allow/Supported —
        // the callee's own, carried through, unless the call declares a set
        // (RFC 3261 §13.2.1/§20.37); provisionals keep passthrough.
        if (200..300).contains(&status) {
            let caps = capabilities::relaying(call, Face::Originator, &passthrough);
            relay::stamp_a_facing_invite_advert(&mut passthrough, &transform.add_headers, &caps);
        }
        let a_rseq = self.own_relayed_rseq(
            call,
            &a_tag,
            i64::from(a_invite.cseq().seq()),
            &source_leg_id,
            resp,
            &mut passthrough,
        );
        let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            &reason,
            Some(a_tag.clone()),
            Some(contact),
            relay_body,
            relay_content_type,
            None,
            passthrough,
            Provenance::Relayed,
        ) else {
            return;
        };
        // A 2xx answers the caller: it goes through the one seam that retains
        // the datagram + arms the §13.3.1.4 ladder.
        if (200..300).contains(&status) {
            self.send_a_leg_answer(call, fx, effect);
        } else {
            // A relayed non-2xx final ends the setup: no 1xx may follow it on
            // the a-leg INVITE transaction (RFC 3261 §17.2.1), so every §3
            // ladder stops with it.
            if status >= 300 {
                self.retire(call, fx, Scope::Provisionals);
            }
            // A reliable provisional leaving under our own number is ours to
            // repeat until PRACKed (RFC 3262 §3): retain it + arm the
            // caller-facing ladder.
            if let Some(a_rseq) = a_rseq.filter(|_| (101..200).contains(&status)) {
                self.arm_reliable_provisional_ladder(call, fx, &effect, &a_tag, a_rseq);
            }
            fx.outbound.push(effect);
        }
    }

    /// Bare-180 downgrade relay ([`crate::rules::model::RuleAction::RelayFirstBare180`]).
    /// The bare 180 ESTABLISHES the a-leg dialog it shows the caller, so that
    /// dialog's local tag is the one source of the owned a-facing To-tag: every
    /// later response to this INVITE — a relayed 18x, the 200, a relayed non-2xx
    /// final, the transaction layer's own 487 — answers under it (RFC 3261
    /// §8.2.6.2). A LATER 18x the `relay18x.messages` policy relays again (ALL /
    /// ONE_PER_VALUE) reads back the same tag, so the caller holds ONE early
    /// dialog regardless of which fork rings. Seed the tag map for this b-leg
    /// dialog, record it (+ the upstream status value for ONE_PER_VALUE dedupe),
    /// then relay the current 1xx as a bare 180. The relay path resolves the
    /// a-facing tag from the map (`find_by_b_tag`).
    pub(super) fn relay_first_bare_180(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        b_tag: &str,
    ) {
        let stored = call::helpers::relay_first_18x_stored_a_tag(call).map(str::to_string);
        let a_facing_tag = self.ensure_a_dialog_with(call, stored);
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
        // A bare 180 states ringing and nothing else: the reliable-provisional
        // negotiation goes with the reliability the downgrade removes (RFC
        // 3262), and the RFC 5009 early-media authorization goes with the media
        // description — authorizing a stream the caller is given no description
        // of leaves it listening to a gate it cannot open.
        let transform = MessageTransform {
            status: Some(180),
            reason: Some("Ringing".to_string()),
            drop_body: true,
            remove_headers: vec![HeaderName::Require, HeaderName::RSeq, HeaderName::PEarlyMedia],
            add_headers: vec![],
        };
        let (peer, target_to_tag) = resolve_peer(call, ctx);
        if let Some(peer) = peer {
            self.relay_to(call, fx, ctx, &peer, &transform, target_to_tag);
        }
    }
}

/// The tag this stack showed the originator of `pending` — its own on the
/// dialog the response is relayed into, read off the originator's `To` (RFC
/// 3261 §12.2.1.1), which the relayed response echoes verbatim and the
/// originator's PRACK names again. A `To` that does not read falls back to
/// the target leg's dialog; `None` when neither names a tag.
fn shown_tag_of(call: &Call, target_leg: &str, pending: &PendingRequest) -> Option<String> {
    To::parse(&SipStr::owned(&pending.source_to))
        .ok()
        .and_then(|to| to.tag().map(str::to_string))
        .or_else(|| {
            call::helpers::find_leg(call, target_leg)?
                .dialogs
                .first()
                .map(|d| d.sip.local_tag.clone())
                .filter(|t| !t.is_empty())
        })
}
/// The headers a response answering the relayed request `pending` states for
/// its originator (RFC 3261 §8.2.6.2): Via / From / To / Call-ID / CSeq equal
/// the originator's own, and the snapshot holds the originator's own bytes
/// (the `call` crate stores text, ADR-0008) — so each rides as a raw entry and
/// reaches the wire unaltered; the requester's `Timestamp`, held on the
/// snapshot, comes back on the response it answers (§8.2.6.1).
pub(super) fn snapshot_response_opts(
    pending: &PendingRequest,
    cseq_method: &str,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    transparent_headers: Vec<SipHeader>,
    contact: Option<sip_message::header::Contact>,
) -> GenerateRelayedResponseOpts {
    let echo = |name: HeaderName, text: &str| Entry::raw(name, SipStr::owned(text));
    GenerateRelayedResponseOpts {
        vias: pending.source_vias.iter().map(|v| echo(HeaderName::Via, v)).collect(),
        record_routes: vec![],
        from: Some(echo(HeaderName::From, &pending.source_from)),
        to: Some(echo(HeaderName::To, &pending.source_to)),
        call_id: Some(echo(HeaderName::CallId, &pending.source_call_id)),
        cseq: Some(echo(HeaderName::CSeq, &format!("{} {}", pending.inbound_cseq, cseq_method))),
        body,
        timestamp: pending.source_timestamp.as_ref().map(|t| echo(HeaderName::Timestamp, t)),
        transparent_headers,
        content_type,
        contact,
    }
}

/// The sent-by a response to this hop is routed to (RFC 3261 §18.2.2). The
/// snapshot the pending-request correlation holds is text (the `call` crate has
/// no sip-message dependency), so reading one back is a parse.
pub(super) fn via_sent_by(via: &str) -> Option<(String, u16)> {
    let hop = Via::parse(&SipStr::owned(via)).ok()?;
    let (host, port) = hop.sent_by().pair();
    Some((host.to_string(), port))
}
