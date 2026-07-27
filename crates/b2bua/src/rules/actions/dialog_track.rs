//! Dialog establishment and tracking: b-leg early dialogs (per callee To-tag,
//! forking-aware), 2xx confirmation, and the a-leg UAS dialog with its stable
//! B2BUA-minted local tag. Relay itself does NOT live here — see
//! [`super::relay_response`] / [`super::relay_request`].

use call::helpers::{find_by_b_tag, set_leg_state};
use call::{B2buaDialogExt, Call, Dialog, LegDisposition, LegState, StackDialog};
use sip_message::header::{self, HeaderValue, RecordRouteEntry};
use sip_message::SipParseError;

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
        if ctx.source_leg_id != leg_id || resp.to().tag() != Some(b_tag) {
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
        let contact =
            contact_uri(resp.header::<header::Contact>(), &call.call_ref, source_leg_id)
                .unwrap_or_default();
        // §12.1.2: an EARLY dialog's route set is established from the reliable
        // 1xx's Record-Route, exactly like the 2xx path below — one entry per
        // recorded route (a comma-combined double-record-route is two), reversed
        // (UAC side). Without this a PRACK/UPDATE on the early dialog rides the
        // preloaded bootstrap Route only and under-reproduces the route set (the
        // §12.2.1.1 audit catches it behind a front proxy).
        let early_route_set =
            self.dialog_route_set(uac_route_set(resp), &call.call_ref, source_leg_id);
        // The response echoes the INVITE's CSeq (§8.1.3.3); seed each forked
        // early dialog's sequence from it so they advance independently.
        let invite_cseq = resp.cseq().seq() as i64;
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
        let remote_tag = resp.to().tag().unwrap_or_default().to_string();
        let remote_tag_clone = remote_tag.clone();
        let remote_target =
            contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
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
        // long-call-loss class). Reading the recorded routes as individual
        // entries puts the proxy's own `;outbound` half on top, so direction is
        // intrinsic to its Record-Route — no `;outbound` worker-stamp and no
        // Via/registry rescue.
        let route_set = self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id);
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
                    d.ext.remote_cseq = Some(resp.cseq().seq() as i64);
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
            _ if !resp.body().is_empty() => Some(resp.body().to_vec()),
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
        let from = a_invite.from();
        let a_leg_id = call.a_leg.leg_id.clone();
        let remote_target =
            contact_uri(a_invite.header::<header::Contact>(), &call.call_ref, &a_leg_id)
                .unwrap_or_else(|| from.uri().to_string());
        // §12.1.1: the a-leg is a UAS dialog — route set is the INVITE's
        // Record-Route entries in forward order, one entry per recorded route (a
        // comma-combined header — the proxy's double-record-route halves — is
        // two), same as the b-leg path above.
        let route_set =
            self.dialog_route_set(uas_route_set(&a_invite), &call.call_ref, &a_leg_id);
        let cseq = a_invite.cseq().seq() as i64;
        let dialog = Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: tag.clone(),
                remote_tag: call.a_leg.from_tag.clone(),
                local_uri: a_invite.to().uri().to_string(),
                remote_uri: from.uri().to_string(),
                remote_target,
                local_cseq: cseq,
                route_set,
            },
            ext: B2buaDialogExt {
                remote_cseq: Some(cseq),
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

    /// The route set to store on a dialog, given a read of the peer's recorded
    /// routes. A recorded route no reader accepts must NOT become an empty
    /// route set: an empty set sends every in-dialog request straight at the
    /// peer's Contact — pod-direct — while the deployment requires every
    /// worker-originated request to traverse the front proxy (a peer's pod IP
    /// is not routable peer-to-peer, so the call is lost the moment it moves).
    /// Such a read falls back to the configured outbound proxy as the dialog's
    /// one route and names the call on stderr; with no proxy configured
    /// (local/dev, where the transport IS peer-direct) the set stays empty.
    fn dialog_route_set(
        &self,
        read: Result<Vec<String>, SipParseError>,
        call_ref: &str,
        leg_id: &str,
    ) -> Vec<String> {
        match read {
            Ok(set) => set,
            Err(err) => {
                let fallback = relay::outbound_proxy_route_set(self.config);
                let route = fallback.first().map(String::as_str).unwrap_or("<none configured>");
                eprintln!(
                    "WARN: call {call_ref} leg {leg_id}: a recorded route does not read ({err}); \
                     dialog route set falls back to the outbound proxy {route} — an empty route \
                     set would send in-dialog requests pod-direct"
                );
                fallback
            }
        }
    }
}

/// The dialog's remote target: the URI of the peer's Contact (RFC 3261
/// §12.1.1/§12.1.2). `None` when the peer sent none; a Contact no reader
/// accepts is named on stderr and leaves the dialog's current target in place
/// rather than silently retargeting it at nothing.
fn contact_uri(
    contact: Option<Result<header::Contact, SipParseError>>,
    call_ref: &str,
    leg_id: &str,
) -> Option<String> {
    match contact? {
        Ok(c) => Some(c.uri().to_string()),
        Err(err) => {
            eprintln!(
                "WARN: call {call_ref} leg {leg_id}: Contact does not read ({err}); keeping the \
                 dialog's current remote target"
            );
            None
        }
    }
}

/// The route set a UAC applies: the responder's recorded routes reversed
/// (RFC 3261 §12.1.2). One entry per recorded route, so a comma-combined line —
/// the front proxy's double record-route — yields both halves in wire order.
/// Errs when a recorded route does not read; the caller decides, and an empty
/// route set is never that decision (see `ActionExecutor::dialog_route_set`).
fn uac_route_set(resp: &sip_message::SipResponse) -> Result<Vec<String>, SipParseError> {
    let mut set = route_texts(resp.list::<RecordRouteEntry>()?);
    set.reverse();
    Ok(set)
}

/// The route set a UAS applies: the requester's recorded routes in the order
/// they were recorded (RFC 3261 §12.1.1). Fallible for the same reason as
/// [`uac_route_set`].
fn uas_route_set(req: &sip_message::SipRequest) -> Result<Vec<String>, SipParseError> {
    Ok(route_texts(req.list::<RecordRouteEntry>()?))
}

/// Recorded routes as the text the `call` crate stores (it has no sip-message
/// dependency, ADR-0008).
fn route_texts(entries: Vec<RecordRouteEntry>) -> Vec<String> {
    entries.into_iter().map(|e| e.to_wire()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::B2buaConfig;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_txn::IdGen;

    const PROXY_ROUTE: &str = "<sip:10.0.0.9:5060;lr>";

    /// A port no reader accepts (RFC 3261 §19.1.1 `port` is 16-bit): the message
    /// parses, the recorded route does not.
    const UNREADABLE_RR: &str = "<sip:10.0.0.9:70596;lr>";

    fn ok_200(record_route: &str) -> sip_message::SipResponse {
        let raw = format!(
            "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:bob@10.244.2.7:5060>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
            SipMessage::Response(r) => r,
            _ => panic!("expected response"),
        }
    }

    fn invite(record_route: &str) -> sip_message::SipRequest {
        let raw = format!(
            "INVITE sip:svc@10.244.1.5:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKa\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@10.0.0.1:5060>;tag=alice\r\n\
To: <sip:svc@10.0.0.9:5060>\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    fn proxied_config() -> B2buaConfig {
        B2buaConfig {
            b2b_outbound_proxy: Some(("10.0.0.9".to_string(), 5060)),
            ..Default::default()
        }
    }

    // A recorded route no reader accepts must never leave the dialog with an
    // EMPTY route set: an empty set sends in-dialog requests at the peer's
    // Contact (pod-direct), which the deployment forbids. Both dialog sides —
    // the UAC route set read off a 2xx/reliable 1xx and the UAS route set read
    // off the a-leg INVITE — fall back to the configured front proxy instead.
    #[test]
    fn an_unreadable_record_route_never_yields_an_empty_route_set() {
        let config = proxied_config();
        let id_gen = IdGen::seeded(0xD1);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

        let resp = ok_200(UNREADABLE_RR);
        assert!(uac_route_set(&resp).is_err(), "the fixture's recorded route must not read");
        assert_eq!(
            exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
            vec![PROXY_ROUTE.to_string()],
            "UAC side: an unreadable recorded route routes via the front proxy, not pod-direct",
        );

        let req = invite(UNREADABLE_RR);
        assert!(uas_route_set(&req).is_err(), "the fixture's recorded route must not read");
        assert_eq!(
            exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
            vec![PROXY_ROUTE.to_string()],
            "UAS side: same fallback",
        );
    }

    // With no front proxy configured (local/dev) the transport IS peer-direct,
    // so the fallback is empty — but the read still fails loudly rather than
    // being mistaken for "the peer recorded no route".
    #[test]
    fn without_a_front_proxy_the_fallback_is_empty_but_the_read_still_fails() {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(0xD2);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
        let resp = ok_200(UNREADABLE_RR);
        assert!(uac_route_set(&resp).is_err());
        assert!(exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1").is_empty());
    }

    // The readable path is unchanged: the UAC reverses the recorded routes
    // (§12.1.2), the UAS keeps them in recorded order (§12.1.1), and a
    // comma-combined line teaches both halves.
    #[test]
    fn readable_recorded_routes_keep_their_dialog_order() {
        let config = proxied_config();
        let id_gen = IdGen::seeded(0xD3);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
        let combined = "<sip:10.0.0.9:5060;outbound;lr>,<sip:10.0.0.9:5060;target=1;lr>";

        let resp = ok_200(combined);
        assert_eq!(
            exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
            vec![
                "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
                "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
            ],
        );

        let req = invite(combined);
        assert_eq!(
            exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
            vec![
                "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
                "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
            ],
        );
    }
}
