//! Relaying an inbound in-dialog **request** to the peer leg: the target
//! dialog's next CSeq, the RAck rewrite and the pending-request correlation
//! snapshot. ACK origination toward a leg is [`super::ack_leg`]'s; response
//! relay is [`super::relay_response`]'s.

use call::helpers::{add_pending_request, bump_our_cseq, find_b_leg, leg_kind};
use call::TxnKind;
use call::{ByeDisposition, Call, DialogId, PendingRequest};
use sip_message::generators::{
    self, CapabilitySet, GenerateInDialogRequestOpts, InDialogMethod, RelayScope, RelaySituation,
};
use sip_message::header::{HeaderName, RAck};
use sip_message::{hops, Method, SipHeader, SipRequest};

use crate::config::B2buaConfig;
use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities;
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::select::{in_dialog_method, invite_cseq_from_handle, leg_at};
use super::unrelayed::Unrelayed;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Relay an inbound SIP request to `target_leg`, in its dialog
    /// `target_dialog`; with no live dialog there to carry it, the sender is
    /// told to retry ([`Unrelayed::NotNow`]). The relayed copy carries the
    /// target dialog's next number (RFC 3261 §12.2.1.1: each dialog numbers
    /// its own requests; a gap in the sender's sequence is not reproduced),
    /// the PRACK `RAck` CSeq rewrite (RFC 3262 §7.2), and the pending-request
    /// snapshot used to correlate the eventual response (RFC 3261 §8.1.3.3).
    pub(super) fn relay_request(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        req: &sip_message::SipRequest,
        target_dialog: Option<DialogId>,
    ) {
        // ACK for 2xx: reuse the INVITE CSeq (no dialog-sequence advance,
        // §13.2.2.4) — delegate to the dedicated builder, carrying the inbound
        // ACK's body (the delayed-offer re-INVITE answer rides the ACK, RFC 3264
        // §4) and its end-to-end headers through, filtered as on any relayed
        // request. The target may be either side (a re-INVITE answered by bob
        // is ACKed toward bob; one answered by alice is ACKed toward alice).
        //
        // Relayed ONLY while the target dialog holds an armed, undischarged ACK
        // obligation for this CSeq — a 2xx in hand that nothing has ACKed yet.
        // Anything else is absorbed: an ACK a rule already composed on this
        // stack's own account is not owed twice, an early dialog owes no ACK (a
        // §17.1.1.3 reject-ACK is hop-local, never relayed), and a surplus peer
        // ACK — fresh Via branch or repeated — must not re-ACK an acknowledged
        // 2xx. A repeated 2xx re-ACKs via `re-ack-retransmitted-2xx`, which never
        // consults this obligation.
        if req.method() == Method::Ack {
            let owed = leg_at(call, target_leg)
                .and_then(|leg| leg.dialogs.established())
                .is_some_and(|d| d.awaited_ack_cseq == Some(req.cseq().seq() as i64));
            if !owed {
                return;
            }
            let content_type = req.raw(HeaderName::ContentType).next().and_then(relay::media_type);
            let face = capabilities::Face::of_leg(target_leg);
            let (extra_headers, _) =
                relayed_request_headers(call, req, target_leg, face, self.config, self.now_ms);
            let caller = relay::Author::Leg(ctx.source_leg_id);
            // The confirmed dialog's offer the 2xx left to this ACK takes the
            // caller's answer in its PRACK; the ACK then carries none.
            if relay::offer_deferred_to_ack(call, target_leg) {
                let answer = relay::ack_answer_on_leg(
                    call,
                    target_leg,
                    req,
                    caller,
                    self.config.sdp_form.as_ref(),
                );
                self.prack_deferred_offer(call, fx, target_leg, answer);
                self.ack_leg(
                    call,
                    fx,
                    target_leg,
                    Vec::new(),
                    None,
                    extra_headers,
                    Provenance::Relayed,
                    caller,
                );
                return;
            }
            let (body, content_type, author) = match self.answer_owed(call, target_leg, req) {
                Some((answer, true)) => (answer, Some(relay::sdp()), caller),
                Some((answer, false)) => (answer, Some(relay::sdp()), relay::Author::Stack),
                None => (req.body().to_vec(), content_type, caller),
            };
            self.ack_leg(
                call,
                fx,
                target_leg,
                body,
                content_type,
                extra_headers,
                Provenance::Relayed,
                author,
            );
            return;
        }
        let Some(method) = in_dialog_method(req.method()) else {
            self.refuse_unrelayable(call, fx, ctx, req);
            return;
        };
        // A relayable request toward a peer dialog a BYE ended or is ending
        // finds no usage left (RFC 3261 §15.1.2; every usage ends with the
        // INVITE usage) while the sender's own dialog lives: it is told to
        // retry, never 481, which would end its dialog (§12.2.1.2). A BYE
        // crosses it.
        if bye_ended(call, target_leg) && *req.method() != Method::Bye {
            self.refuse_unrelayed(call, fx, ctx, Unrelayed::NotNow);
            return;
        }
        // The dialog the resolver named — the callee fork toward a b-leg, the
        // caller-facing dialog toward the a-leg (RFC 3261 §12.2.1.1: each
        // early dialog is independent). One that carries requests takes it;
        // with none, the sender is told to retry (`Unrelayed::NotNow`).
        let picked = leg_at(call, target_leg).and_then(|leg| {
            let id = target_dialog.as_ref()?;
            let d = leg.dialogs.by_tags(&id.our_tag, &id.peer_tag)?;
            d.carries_requests().then(|| d.clone())
        });
        let Some(target_dialog) = picked else {
            self.refuse_unrelayed(call, fx, ctx, Unrelayed::NotNow);
            return;
        };

        // The target dialog's next number (§12.2.1.1).
        let inbound_cseq = req.cseq().seq() as i64;
        let target_invite_cseq =
            invite_cseq_from_handle(&target_dialog).unwrap_or(target_dialog.our_cseq);
        let outbound_cseq = target_dialog.our_cseq + 1;
        let t_id = target_dialog.id();
        // The call as it stands before the relay records anything: a request
        // that is not built leaves it so.
        let unrelayed = call.clone();
        *call = bump_our_cseq(call.clone(), target_leg, &t_id, 1);

        // RFC 3262 §7.2: RAck names the reliable 1xx and the INVITE that drew it
        // *on the target leg* — the CSeq token becomes the target's INVITE CSeq
        // and the RSeq token translates back to what the target stated.
        // `relay-prack` relays only a PRACK naming a provisional this stack
        // showed (`call::helpers::prack_disposition`); the responder's PRACK
        // is then the relayed one, and the offer it answers no longer owed.
        let rack = if method == InDialogMethod::Prack {
            // The number the PRACKing party names is unique to the dialog it
            // was shown in, so the translation is keyed there — and the PRACK
            // names that dialog itself: its To-tag is the tag this stack showed
            // on its face, whichever leg the provisional came from.
            let shown_tag = ctx.dialog().map_or(String::new(), |d| d.our_tag.clone());
            req.header::<RAck>().and_then(Result::ok).map(|r| {
                let responder =
                    call::helpers::responder_of(call, &shown_tag, call::RSeq(r.rseq())).cloned();
                let rseq = match responder {
                    Some(id) => {
                        call::helpers::note_responder_ack(
                            call,
                            &id,
                            call::ResponderAck::RelayedPrack,
                        );
                        super::prack::clear_owed_offer(call, &id);
                        id.rseq.0
                    }
                    None => r.rseq(),
                };
                RAck::new(rseq, relay::wire_cseq(target_invite_cseq), r.method().clone())
            })
        } else {
            None
        };
        let content_type = req.raw(HeaderName::ContentType).next().and_then(relay::media_type);
        let body = relay::continue_on_leg(
            call,
            target_leg,
            Some(&target_dialog.peer_tag),
            relay::Author::Leg(ctx.source_leg_id),
            relay::Carried::of(req.method(), None, true),
            req.body().to_vec(),
            content_type.as_ref(),
            self.config.sdp_form.as_ref(),
        );
        let branch = self.turn.ids.new_branch();
        let Some(target) = leg_at(call, target_leg) else {
            *call = unrelayed;
            self.leg_gone(call, target_leg, req.method().as_str());
            return;
        };
        let gen_dialog = relay::to_gen_dialog(target, &target_dialog);
        let target_face = capabilities::Face::of_leg(target_leg);
        let (extra_headers, advertised) =
            relayed_request_headers(call, req, target_leg, target_face, self.config, self.now_ms);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(
                self.config,
                relay::CallMarks::of(call),
                target_leg,
                branch.clone(),
            )),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), target_leg)),
            body,
            content_type,
            cseq: Some(relay::wire_cseq(outbound_cseq)),
            // §16.6 step 3: the relayed request continues the sender's hop
            // budget. The stack's OWN in-dialog requests (teardown BYE,
            // keepalive, a re-offer it authors) state the §8.1.1.6 default —
            // only what crosses the back-to-back UA inherits a count.
            max_forwards: Some(hops::forwarded_max_forwards(req).value()),
            extra_headers,
            rack,
            // The peer's own advertisement rides in `extra_headers`; this is
            // the DECLARED set for the face, else nothing of the stack's own.
            capabilities: Some(advertised),
            ..Default::default()
        };
        let res = match generators::generate_in_dialog_request(method, &gen_dialog, &opts) {
            Ok(res) => res,
            Err(e) => {
                *call = unrelayed;
                tracing::warn!(call_ref = %call.call_ref, leg_id = %target_leg,
                    method = %req.method(), error = %e,
                    "in-dialog request not relayed: its lines make no request toward the peer");
                self.refuse_unrelayed(call, fx, ctx, Unrelayed::Unbuildable);
                return;
            }
        };
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            target_leg,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        let kind =
            if method == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };

        // For a re-INVITE, cache its client-transaction handle on the *target*
        // dialog so the eventual ACK-for-2xx echoes the re-INVITE CSeq
        // (RFC 3261 §13.2.2.4) and CANCEL can reuse the branch (§9.1). (The
        // initial INVITE never reaches this path — it is built by
        // `CreateLeg`/`build_b_leg`.)
        if method == InDialogMethod::Invite {
            *call = call::helpers::update_dialog(call.clone(), target_leg, &t_id, |d| {
                d.pending_invite_txn = Some(call::InviteTxnHandle {
                    branch: branch.clone(),
                    cseq: i64::from(out_req.cseq().seq()),
                    original_invite: out_req.image().to_vec(),
                    destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
                });
                // New INVITE transaction → drop the prior ACK branch and its
                // retained datagram (§13.2.2.4); this re-INVITE's 2xx ACK mints
                // its own, retained on first ACK — a copy of that 2xx must
                // never be re-ACKed with the PREVIOUS transaction's answer.
                // Any stale relay-ACK obligation dies with it — this 2xx arms
                // its own (`relay_response`).
                d.ack_branch = None;
                d.emitted_ack = None;
                d.awaited_ack_cseq = None;
            });
        }

        // Snapshot the inbound request so the response can echo its Via/From/To/
        // Call-ID/CSeq (§8.1.3.3) and so glare detection on the target dialog
        // sees the in-flight re-INVITE (`reinvite-glare`). The B2BUA answers BYE
        // locally and ACK has no response, so neither needs correlation.
        if !matches!(method, InDialogMethod::Bye) {
            let pending = PendingRequest {
                method: req.method().to_string(),
                outbound_cseq,
                inbound_cseq,
                source_vias: req.raw(HeaderName::Via).map(str::to_string).collect(),
                source_call_id: req.call_id().as_str().to_string(),
                source_from: req.raw(HeaderName::From).next().unwrap_or_default().to_string(),
                source_to: req.raw(HeaderName::To).next().unwrap_or_default().to_string(),
                direction: ctx.direction,
                cancelled: false,
                offered_100rel: req.offers_100rel(),
                offered: relay::carries_sdp(req),
                source_timestamp: req.raw(HeaderName::Timestamp).next().map(str::to_string),
                relayed_by: self.config.self_ordinal.clone(),
            };
            *call = add_pending_request(call.clone(), target_leg, &t_id, pending);
        }

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("relay {} → {target_leg}", req.method()),
            leg_id: Some(target_leg.to_string()),
            provenance: Provenance::Relayed,
        });
    }
}

impl ActionExecutor<'_> {
    /// Answer `req`, a request of a method this stack relays in no dialog,
    /// so its sender is not left to run out Timer F (RFC 3261 §8.2,
    /// §17.2.2): a SUBSCRIBE draws `489 Bad Event`, as this stack serves no
    /// event package (RFC 6665 §4.2.1.2); any other method `405` with the
    /// node's `Allow` (§8.2.1), the node's answer to a method it does not
    /// serve.
    pub(super) fn refuse_unrelayable(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        req: &SipRequest,
    ) {
        if req.method() == Method::Subscribe {
            self.respond(call, fx, ctx, 489, "Bad Event", &[], None, vec![]);
            return;
        }
        let allow = self
            .config
            .node_capabilities
            .allow_text()
            .unwrap_or_else(|| generators::B2BUA_ALLOW.to_string());
        let allow = SipHeader {
            name: sip_message::SipStr::owned(HeaderName::Allow.as_wire_str()),
            value: sip_message::SipStr::owned(&allow),
        };
        self.respond(call, fx, ctx, 405, "Method Not Allowed", &[], None, vec![allow]);
    }
}

/// Whether a BYE ended `leg_id`'s dialog: the peer's BYE answered, or one this
/// stack sent.
fn bye_ended(call: &Call, leg_id: &str) -> bool {
    call::helpers::find_leg(call, leg_id).and_then(|l| l.end()).is_some_and(|d| {
        matches!(
            d,
            ByeDisposition::ByeSent
                | ByeDisposition::ByeReceived
                | ByeDisposition::ByeConfirmed
                | ByeDisposition::ByeTimeout
        )
    })
}

/// What a request relayed in dialog toward `target_leg` carries, and the
/// capability set the generator stamps for that face. The source's end-to-end
/// headers ride bar the halves the face declares and the advertisement headers
/// the method does not admit. The call's withheld option
/// tags govern the INVITEs the stack originates; on a relayed request the
/// peers negotiate end to end (RFC 3262 per transaction), so only their
/// `timer` half reaches it, toward a leg the call originated; and the session
/// timer rides only to a leg dialled into it, as far as the leg offers it
/// (`relay::settle_session_timer`). A `Timestamp` the source stated is
/// restated with the turn's clock, `now_ms` (RFC 3261 §20.38).
fn relayed_request_headers(
    call: &Call,
    req: &SipRequest,
    target_leg: &str,
    face: capabilities::Face,
    config: &B2buaConfig,
    now_ms: i64,
) -> (Vec<SipHeader>, CapabilitySet) {
    let declared = capabilities::declared_advert_headers(call.features.as_ref(), face);
    let situation = RelaySituation::request(req.method(), relay::toward_leg(target_leg));
    let stamp = generators::timestamp_value(now_ms);
    let scope = config.relay_scope(RelayScope::request(), situation).stamped(Some(&stamp));
    let mut headers = b2bua_sdk::in_dialog_relay::relayed_request_lines(req, &declared, scope);
    let mut advertised = capabilities::advertised(call, face);
    let leg = find_b_leg(call, target_leg);
    let withheld = match (face, leg) {
        // The offer state decides only `100rel`, which never reaches here.
        (capabilities::Face::Originated, Some(leg)) => {
            capabilities::withheld_option_tags(call, Some(leg_kind(leg)), true)
                .into_iter()
                .filter(|tag| tag.eq_ignore_ascii_case(relay::TIMER))
                .collect()
        }
        _ => Vec::new(),
    };
    relay::apply_withheld_option_tags(&mut headers, &withheld);
    for tag in &withheld {
        advertised = advertised.without_option_tag(tag);
    }
    // The leg's part was fixed when it was dialled, the call's `timer` withhold
    // and the strategy's included.
    let takes_part = leg.and_then(|l| l.in_session_timer) != Some(false);
    let leg_offers = if declared.contains(&HeaderName::Supported) {
        advertised.supported().is_some_and(|s| s.contains(relay::TIMER))
    } else {
        generators::offers_session_timer(&headers)
    };
    relay::settle_session_timer(&mut headers, req.headers(), takes_part, leg_offers);
    (headers, advertised)
}
