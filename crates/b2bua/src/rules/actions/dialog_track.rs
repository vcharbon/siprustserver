//! Dialog establishment and tracking: b-leg early dialogs (per callee To-tag,
//! forking-aware), 2xx confirmation, and the a-leg UAS dialog with its stable
//! B2BUA-minted local tag. Relay itself does NOT live here — see
//! [`super::relay_response`] / [`super::relay_request`].

use call::helpers::{find_by_b_tag, set_leg_state};
use call::{B2buaDialogExt, Call, Dialog, LegDisposition, LegState, StackDialog};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::structured_headers::split_top_level_commas;

use crate::rules::model::RuleContext;
use crate::rules::relay;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Register the current event's b-leg early dialog when the machine handles
    /// a provisional WITHOUT relaying it (a suppressed fork's reliable 1xx):
    /// `track_b_early_dialog` otherwise only runs on the relay path, so a
    /// suppressed fork would have no `(leg, b_tag)` dialog and its AS-originated
    /// PRACK / SDP cache — keyed strictly on that pair — would silently miss
    /// (a fallback to the FIRST dialog mis-targets the PRACK and overwrites
    /// another fork's cached answer). Only the event's own response can seed the
    /// dialog (Contact / Record-Route / CSeq come from it); other `(leg, tag)`
    /// targets must already be tracked. Idempotent (`track_b_early_dialog`
    /// skips a known tag).
    pub(super) fn ensure_b_early_dialog(&self, call: &mut Call, ctx: &RuleContext, leg_id: &str, b_tag: &str) {
        if b_tag.is_empty() || leg_id == "a" {
            return;
        }
        let Some(resp) = ctx.response() else { return };
        if ctx.source_leg_id != leg_id || resp.to.tag.as_deref() != Some(b_tag) {
            return;
        }
        self.track_b_early_dialog(call, leg_id, resp, b_tag);
    }

    /// Establish (or refresh) a b-leg early dialog from a reliable 1xx so a
    /// subsequent in-dialog request (PRACK/UPDATE) can target the callee with
    /// the right To-tag (RFC 3261 §12.1.2). One early dialog per distinct
    /// callee To-tag (downstream forking → several per b-leg); called from the
    /// b-leg 1xx/2xx relay path and from [`Self::ensure_b_early_dialog`] (the
    /// suppressed-provisional seam). Idempotent per tag.
    pub(super) fn track_b_early_dialog(
        &self,
        call: &mut Call,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        to_tag: &str,
    ) {
        let contact = get_header(&resp.headers, "contact").map(unwrap_angle).unwrap_or_default();
        // §12.1.2: an EARLY dialog's route set is established from the reliable
        // 1xx's Record-Route, exactly like the 2xx path below — split the
        // comma-combined double-record-route halves first, then reverse the
        // individual URIs (UAC side). Without this a PRACK/UPDATE on the early
        // dialog rides the preloaded bootstrap Route only and under-reproduces
        // the route set (the §12.2.1.1 audit catches it behind a front proxy).
        let mut early_route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&resp.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        early_route_set.reverse();
        // The response echoes the INVITE's CSeq (§8.1.3.3); seed each forked
        // early dialog's sequence from it so they advance independently.
        let invite_cseq = resp.cseq.seq as i64;
        let already = call
            .b_legs
            .iter()
            .find(|l| l.leg_id == source_leg_id)
            .map(|l| l.dialogs.iter().any(|d| d.sip.remote_tag == to_tag))
            .unwrap_or(false);
        if already {
            return;
        }
        *call = call::helpers::update_leg(call.clone(), source_leg_id, |leg| {
            leg.state = LegState::Early;
            if let Some(d) = leg.dialogs.iter_mut().find(|d| d.sip.remote_tag.is_empty()) {
                // First real fork: seed the placeholder dialog in place so it
                // keeps its pending INVITE handle (ACK-for-2xx / RAck CSeq).
                d.sip.remote_tag = to_tag.to_string();
                if !contact.is_empty() {
                    d.sip.remote_target = contact.clone();
                }
                if !early_route_set.is_empty() {
                    d.sip.route_set = early_route_set.clone();
                }
            } else {
                // Additional fork: append a fresh independent early dialog
                // (RFC 3261 §12.1.2). All forks share the one INVITE the B2BUA
                // sent, so this dialog inherits the leg's initial-INVITE handle —
                // otherwise its 2xx ACK (and RAck CSeq) would fall back to the
                // running `local_cseq`, which any early PRACK/UPDATE has already
                // advanced past the INVITE (§13.2.2.4 wants the INVITE's CSeq).
                let leg_handle = leg
                    .dialogs
                    .iter()
                    .find_map(|d| d.ext.pending_invite_txn.clone());
                let ctx = call::helpers::MakeDialogLegCtx {
                    call_id: &leg.call_id,
                    local_uri: leg.local_uri.as_deref().unwrap_or(""),
                    remote_uri: leg.remote_uri.as_deref().unwrap_or(""),
                    local_tag: &leg.from_tag,
                    remote_tag: to_tag,
                };
                let mut d = call::helpers::make_empty_dialog(&ctx, invite_cseq);
                d.ext.pending_invite_txn = leg_handle;
                if !contact.is_empty() {
                    d.sip.remote_target = contact.clone();
                }
                if !early_route_set.is_empty() {
                    d.sip.route_set = early_route_set.clone();
                }
                leg.dialogs.push(d);
            }
        });
    }

    /// Confirm a b-leg dialog from the 2xx response event (learn remote tag /
    /// target / CSeq), mark it confirmed+bridged, and ensure the a-leg dialog
    /// exists.
    pub(super) fn confirm_dialog(&self, call: &mut Call, ctx: &RuleContext, leg_id: &str) {
        let resp = match ctx.response() {
            Some(r) => r,
            None => return,
        };
        let remote_tag = resp.to.tag.clone().unwrap_or_default();
        let remote_tag_clone = remote_tag.clone();
        let remote_target = get_header(&resp.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_default();
        // §12.1.2: the b-leg is a UAC dialog, so its route set is the
        // dialog-creating 2xx's Record-Route values in *reverse* order (the
        // a-leg/UAS path keeps the INVITE's Record-Route forward). We must reverse
        // *individual route URIs*, not header lines: the front proxy double-record-
        // routes (a `;outbound` half + a cookie half), and on the wire those two
        // arrive comma-combined in a single Record-Route header (RFC 3261 §7.3.1).
        // Reversing per-header would be a no-op on that single value and leave the
        // cookie on top — so the worker→callee keepalive carries the cookie first,
        // the proxy decodes it (`w_pri`) and bounces the request back to a worker
        // after a reboot onto a new pod IP the registry has not yet learned (the
        // long-call-loss class). Split top-level commas first so the proxy's own
        // `;outbound` half lands on top and direction is intrinsic to its
        // Record-Route — no `;outbound` worker-stamp and no Via/registry rescue.
        let mut route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&resp.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        route_set.reverse();
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            // Forking (RFC 3261 §12.1.2): the 2xx confirms exactly ONE early
            // dialog — the one whose callee tag it carries. Promote *that* fork
            // (it already holds its own CSeq sequence, advanced by any early
            // PRACK/UPDATE) and discard the losing early dialogs. Confirming
            // `dialogs.first()` unconditionally would resurrect fork 1's stale
            // CSeq under fork 2's tag, so the next in-dialog request re-uses a
            // number the winning fork already spent (§12.2.1.1 violation).
            let idx = leg
                .dialogs
                .iter()
                .position(|d| !remote_tag.is_empty() && d.sip.remote_tag == remote_tag)
                .or_else(|| leg.dialogs.iter().position(|d| d.sip.remote_tag.is_empty()))
                .unwrap_or(0);
            if idx < leg.dialogs.len() {
                {
                    let d = &mut leg.dialogs[idx];
                    if !remote_tag.is_empty() {
                        d.sip.remote_tag = remote_tag.to_string();
                    }
                    if !remote_target.is_empty() {
                        d.sip.remote_target = remote_target;
                    }
                    if !route_set.is_empty() {
                        d.sip.route_set = route_set;
                    }
                    d.ext.remote_cseq = Some(resp.cseq.seq as i64);
                }
                // One dialog survives confirmation (model: "one survives after
                // confirmed") — drop the losing forks so per-call state is bounded.
                let winner = leg.dialogs.remove(idx);
                leg.dialogs = vec![winner];
            }
            leg.state = LegState::Confirmed;
            leg.disposition = LegDisposition::Bridged;
        }
        // Reuse the a-facing tag pre-seeded for this callee (relayFirst18x's
        // `force-tag-consistency`) so the 200 OK To-tag matches the first 180.
        let preferred = find_by_b_tag(call, leg_id, &remote_tag_clone).map(|m| m.a_tag.clone());
        self.ensure_a_dialog_with(call, preferred.clone());
        // When a *non-first* fork wins, the a-dialog was already created under
        // the first fork's primary tag; adopt the winning fork's a-face tag so
        // the confirmed a-dialog matches the To-tag the caller saw on the 2xx
        // (and any B2BUA-originated a-facing in-dialog request uses it).
        if let (Some(pref), Some(d)) = (preferred, call.a_leg.dialogs.first_mut()) {
            if !pref.is_empty() {
                d.sip.local_tag = pref;
            }
        }
        // Stash the answer SDP that was relayed toward alice on the 2xx so an
        // un-ACKed-2xx retransmit (RFC 3261 §13.3.1.4) re-sends a faithful copy —
        // the caller that lost the original 200 needs its answer. Mirror the relay
        // body choice (policy override else the callee's 200 body).
        let answer_body = match call.policy_update_body.clone() {
            Some(call::PolicyUpdateBody::Bytes(b)) => Some(b),
            _ if !resp.body.is_empty() => Some(resp.body.to_vec()),
            _ => None,
        };
        if let (Some(body), Some(d)) = (answer_body, call.a_leg.dialogs.first_mut()) {
            d.ext.cached_sdp = Some(body);
        }
        // Bridge the a-leg only for a live answer that actually faces the caller.
        // Two guards, both load-bearing:
        //
        // 1. `call.state == Active`. `cancel-200-crossing` reuses `confirm_dialog`
        //    to learn the crossing 200's dialog so it can ACK + BYE the abandoned
        //    callee, but on the reject/no-failover teardown the call is already
        //    `Terminating` and the a-leg was never answered — resurrecting it to
        //    `Confirmed` would read as "answered" and suppress the ADR-0022
        //    unanswered-a-leg final (`invariants::enforce`), stranding the caller
        //    with no response. Leave it as-is (Trying/Early) so the caller still
        //    gets its final when the deferred termination finalizes. The failover
        //    crossing-200 reap runs while the call is still `Active` (awaiting
        //    `/call/failure`), so it keeps confirming the a-leg as before.
        //
        // 2. The confirmed leg is *adopted*. An unadopted `media` leg (an MRF
        //    parked behind early media, ADR-0016) is answered by the *service*,
        //    not relayed to alice — its 2xx surfaced to the caller only as a 183,
        //    so RFC-wise there is no confirmed a-dialog. Confirming the a-leg off
        //    such a 2xx would let a later `BeginTermination` BYE a dialog the
        //    caller never established (undeliverable BYE → the call strands in
        //    `Terminating`, its CDR never flushes). The a-leg is confirmed only
        //    by an a-facing final 2xx — core relay, `RespondToALeg`, or
        //    `AnswerALegNewDialog`. Adopted destination legs (incl. the REFER
        //    transfer target and the failover crossing-200 callee) are unaffected.
        let confirmed_leg_adopted = call
            .b_legs
            .iter()
            .find(|l| l.leg_id == leg_id)
            .is_none_or(call::helpers::is_adopted);
        if call.state == call::CallModelState::Active && confirmed_leg_adopted {
            *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
        }
    }

    /// Ensure the a-leg has a dialog with a stable B2BUA-minted local tag; return
    /// that tag (the To-tag presented to alice on every a-facing response).
    pub(super) fn ensure_a_dialog(&self, call: &mut Call) -> String {
        self.ensure_a_dialog_with(call, None)
    }

    /// Like [`Self::ensure_a_dialog`] but, when the a-dialog is being created,
    /// uses `preferred` as its local tag instead of minting a fresh one (tag
    /// continuity across forking/failover, `relayFirst18xTo180`).
    pub(super) fn ensure_a_dialog_with(&self, call: &mut Call, preferred: Option<String>) -> String {
        if let Some(d) = call.a_leg.dialogs.first() {
            if !d.sip.local_tag.is_empty() {
                return d.sip.local_tag.clone();
            }
        }
        let tag = preferred.unwrap_or_else(|| self.id_gen.new_tag());
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let remote_target = get_header(&a_invite.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| a_invite.from.uri.to_string());
        // §12.1.1: the a-leg is a UAS dialog — route set is the INVITE's
        // Record-Route values in forward order. Split top-level commas so a
        // comma-combined header (the proxy's double-record-route halves) becomes
        // individual route URIs, same as the b-leg path above.
        let route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&a_invite.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        let dialog = Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: tag.clone(),
                remote_tag: call.a_leg.from_tag.clone(),
                local_uri: a_invite.to.uri.to_string(),
                remote_uri: a_invite.from.uri.to_string(),
                remote_target,
                local_cseq: a_invite.cseq.seq as i64,
                route_set,
            },
            ext: B2buaDialogExt {
                remote_cseq: Some(a_invite.cseq.seq as i64),
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
                pending_reinvite_2xx: None,
            },
        };
        call.a_leg.dialogs = vec![dialog];
        tag
    }
}

fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}
