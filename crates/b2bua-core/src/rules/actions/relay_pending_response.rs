//! Relaying a response that answers a request this stack relayed (§8.1.3.3):
//! rebuilt from the snapshot taken as the request was relayed, so it echoes
//! the originator's Via/From/To/CSeq.

use call::helpers::{PendingHolder, Scope};
use call::{Call, PendingRequest, RSeq};
use sip_message::generators::{self, RelayScope, RelaySituation};
use sip_message::header::{From, HeaderValue};
use sip_message::{Method, SipStr};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities;
use crate::rules::relay;

use super::relay_response::Relaying;
use super::show_provisional::shown_tag_of;
use super::snapshot_answer::{snapshot_response_opts, via_sent_by};
use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// Relay `r`, the response to the relayed request `pending` kept by
    /// `holder`, toward its originator; a reliable provisional leaves under
    /// this stack's own `RSeq` where RFC 3262 admits one at all.
    pub(super) fn relay_pending_response(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        r: &Relaying<'_>,
        holder: PendingHolder,
        pending: PendingRequest,
    ) {
        let (status, target_leg, source_leg_id) =
            (r.status, r.target_leg, r.source_leg_id.as_str());
        // §18.2.2: a relayed response goes to the originator's top Via
        // sent-by, and the snapshot holds that Via verbatim. A Via no
        // reader accepts names no address — drop the relay and say so
        // (the originator retransmits, then times out its own request).
        // Answering it toward a fabricated destination would post the
        // caller's response to whatever that address happens to be; a
        // loopback fallback would swallow it silently.
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
        // or answers a target refresh (`response_states_own_contact`) —
        // a relayed 200 to PRACK/OPTIONS/INFO/MESSAGE states none, and a
        // relayed 3xx / 485 carries the peer's targets in passthrough.
        let contact =
            generators::response_states_own_contact(&Method::from_wire(&r.cseq_method), status)
                .then(|| relay::leg_contact(self.config, relay::CallMarks::of(call), target_leg));
        let mut transparent_headers = r.passthrough(relay::relay_response_passthrough_headers(
            r.resp,
            self.config
                .relay_scope(
                    RelayScope::response_carrying(r.source_body),
                    RelaySituation::response(
                        status,
                        r.resp.cseq().method(),
                        relay::toward_leg(target_leg),
                    ),
                )
                .stamped(pending.source_timestamp.as_deref()),
        ));
        // A 2xx answer to a B2BUA-relayed re-INVITE advertises this
        // face's capability set (RFC 3261 §13.2.1/§20.37) — the source
        // response's own, carried through, unless the call declares one;
        // a 2xx to OPTIONS states this UA's own set (§11.2). Other
        // responses keep verbatim passthrough.
        if r.cseq_method == "INVITE" && (200..300).contains(&status) {
            let caps = capabilities::relaying_for_leg(call, target_leg, &transparent_headers);
            relay::stamp_advertisement(&mut transparent_headers, &r.transform.add_headers, &caps);
        }
        if r.cseq_method == "OPTIONS" && (200..300).contains(&status) {
            relay::stamp_options_advertisement(
                &mut transparent_headers,
                &r.transform.add_headers,
                &self.config.node_capabilities,
            );
        }
        // A reliable provisional leaves toward the originator under
        // THIS stack's number — it is the UAS of that face, and RFC
        // 3262 §3 makes the sequence the sender's, per transaction —
        // recorded so the PRACK naming it translates back, and only
        // where §3 admits one at all (`admits_reliable_provisional`:
        // an INVITE whose originator offered `100rel`). Otherwise it
        // leaves as an ordinary provisional, shown to no one, and the
        // engine PRACKs the responder: it offered the extension there,
        // the originator did not.
        let mut ladder: Option<(String, RSeq)> = None;
        if relay::reliable_rseq(r.resp).is_some() && (101..200).contains(&status) {
            let shown_tag = call::helpers::admits_reliable_provisional(&pending)
                .then(|| shown_tag_of(&pending))
                .flatten();
            match shown_tag {
                Some(shown_tag) => {
                    ladder = self
                        .own_relayed_rseq(
                            call,
                            true,
                            &shown_tag,
                            pending.inbound_cseq,
                            source_leg_id,
                            r.resp,
                            &mut transparent_headers,
                            r.carried_sdp,
                        )
                        .map(|rseq| (shown_tag, rseq));
                }
                None => relay::strip_reliability(&mut transparent_headers),
            }
        }
        // The dialog the answered request came in on: its sender's tag. A
        // relay of a dialog the answer retired leaves as written: its
        // description belongs to no session this stack keeps, and never
        // moves the answered one's (RFC 3264 §8).
        let dialog = From::parse(&SipStr::owned(&pending.source_from))
            .ok()
            .and_then(|from| from.tag().map(str::to_string))
            .map(|peer_tag| call::DialogId {
                our_tag: shown_tag_of(&pending).unwrap_or_default(),
                peer_tag,
            });
        let relay_body = match &holder {
            PendingHolder::Retired(_) => r.body.clone(),
            PendingHolder::Dialog(_) => relay::continue_on_leg(
                call,
                target_leg,
                dialog.as_ref(),
                relay::Author::Leg(source_leg_id),
                relay::Carried::answering(&Method::from_wire(&r.cseq_method), status),
                r.body.clone(),
                r.content_type.as_ref(),
                self.config.sdp_form.as_ref(),
            ),
        };
        let opts = snapshot_response_opts(
            &pending,
            &r.cseq_method,
            relay_body,
            r.content_type.clone(),
            transparent_headers,
            contact,
        );
        let Ok(mut relayed) = generators::generate_relayed_response(status, &r.reason, &opts)
        else {
            tracing::warn!(
                call_ref = %call.call_ref,
                leg_id = %source_leg_id,
                status,
                "relayed response dropped — the originator's request lines make no response"
            );
            return;
        };
        if status >= 200 {
            self.close_relayed_transaction(call, fx, r, &holder);
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
        if r.cseq_method == "INVITE" && (200..300).contains(&status) {
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
                source_leg_id,
                Some(pending.inbound_cseq),
            );
            self.retain_reinvite_2xx(call, fx, &mut relayed, dest.clone(), target_leg);
        }
        let mut effect = OutboundSipEffect {
            body: OutboundBody::Response(relayed),
            mode: OutboundTxnMode::ServerResponse,
            destination: dest,
            label: format!("{status} {} → {target_leg}", r.cseq_method),
            leg_id: Some(target_leg.to_string()),
            provenance: Provenance::Relayed,
        };
        // A reliable provisional leaving under our own number is ours
        // to repeat until PRACKed (RFC 3262 §3): retain it + arm the
        // ladder on the face it left on.
        if let Some((shown_tag, rseq)) = ladder {
            self.arm_reliable_provisional_ladder(call, fx, &mut effect, &shown_tag, rseq);
        }
        fx.outbound.push(effect);
    }

    /// §8.1.3.3 / §14.1: the snapshot correlates EVERY response of the
    /// transaction back to the originator, so it outlives a relayed
    /// provisional (a re-INVITE may send 18x THEN a non-2xx final) and is
    /// removed on the final `r`, after which the transaction has no response.
    fn close_relayed_transaction(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        r: &Relaying<'_>,
        holder: &PendingHolder,
    ) {
        let source_leg_id = r.source_leg_id.as_str();
        // The final ends this transaction: the PRACKs its responder
        // is owed leave first, a 2xx leaving the offer of the dialog
        // it confirms to the ACK (RFC 3262 §4, §5), and its
        // provisionals stop repeating on the face they were shown
        // (§3, RFC 3261 §17.2.1). This transaction's alone — a
        // re-INVITE's final is not the setup's.
        if r.cseq_method == "INVITE" {
            let confirmed = (200..300).contains(&r.status).then_some(r.to_tag.as_str());
            self.settle_owed_pracks(
                call,
                fx,
                Owed::Invite { leg: source_leg_id, cseq: r.cseq_num },
                Settle::AtFinal { confirmed },
            );
            self.retire(call, fx, Scope::Transaction { leg_id: source_leg_id, cseq: r.cseq_num });
        }
        *call = call::helpers::close_relay(call.clone(), source_leg_id, holder, r.cseq_num);
    }
}
