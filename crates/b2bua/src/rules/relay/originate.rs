//! [`build_b_leg`] — the single mint point for every leg the B2BUA originates:
//! the callee b-leg, and the REFER transfer leg (fed the caller's rehydrated
//! INVITE via [`rebuild_a_leg_invite`]). The originated leg's dialog identity
//! (Call-ID, tags, CSeq space, Contact) is minted fresh — nothing
//! dialog-identifying is copied from the a-leg.

use call::{
    B2buaDialogExt, Dialog, InviteTxnHandle, Leg, LegDisposition, LegState, RemoteInfo, StackDialog,
};
use sip_message::draft::RequestDraft;
use sip_message::generators::{
    self, CapabilitySet, GenerateOutOfDialogRequestOpts, OutOfDialogMethod, RelayScope,
};
use sip_message::header::{
    self, ChargingVector, HeaderClass, HeaderName, HeaderValue, MaxForwards, NameAddr,
    TokenListHeader, Uri,
};
use sip_message::{hops, Method, SipHeader as MsgHeader, SipRequest, SipStr};
use sip_txn::{IdGen, TxnKind};

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance};

use super::address::{address, identity, UnreadableAddress};
use super::body::{media_type, sdp};
use super::egress::apply_b_leg_egress;
use super::identity::{leg_contact, leg_via};

/// Rebuild the a-leg's original INVITE as a `SipRequest` (for `generate_response`).
/// Every header rides as an unparsed line, so the rebuilt message carries the
/// caller's bytes exactly as they arrived. A caller that omitted `Max-Forwards`
/// gets RFC 3261 §8.1.1.6's default, which is the count [`build_b_leg`] then
/// carries onto the transfer leg it originates from this rebuild.
///
/// The Request-URI is read verbatim, not refused: this is a round-trip of text
/// the inbound parser's own strict gates already admitted, so a refusal here
/// would drop a call the stack accepted. Nothing routes on it — the b-leg's
/// Request-URI comes from the decision, through [`build_b_leg`]'s reader.
pub fn rebuild_a_leg_invite(snap: &call::ALegInviteSnapshot) -> SipRequest {
    let mut draft =
        RequestDraft::new(Method::Invite, Uri::parse_or_verbatim(&SipStr::owned(&snap.uri)));
    for h in &snap.headers {
        draft = draft.push_raw(HeaderName::from(h.name.as_str()), SipStr::owned(&h.value));
    }
    if !draft.has(&HeaderName::MaxForwards) {
        draft = draft.push(MaxForwards::DEFAULT);
    }
    draft
        .with_body(snap.body.clone().into())
        .freeze()
        .expect("a-leg INVITE snapshot is well-formed")
}

/// The transparency scope of the INVITE this B2BUA originates: a decision that
/// replaces the originator's body states a body of the same role (a held REFER
/// offer) or none at all, and the set describing the originator's body rides
/// only as far as what replaced it still answers for.
fn relay_scope(body_override: Option<&[u8]>) -> RelayScope {
    let scope = RelayScope::request();
    match body_override {
        Some(body) if body.is_empty() => scope.without_source_body(),
        Some(_) => scope.with_replaced_body(),
        None => scope,
    }
}

/// True iff `header_updates` names `header` with no value — a caller stating
/// that this name does not ride, which the §16.6 relay and the configured
/// carry-through both honour so a withheld name has one meaning on every path.
fn removed(header_updates: &[(String, Option<String>)], header: &HeaderName) -> bool {
    header_updates.iter().any(|(name, value)| value.is_none() && header.matches(name))
}

/// The call's own offer on an originated leg's assembled headers: the
/// `Supported` set states every tag in `offered`, once — an absent line comes
/// into being for them, a line already naming them all is left byte-identical.
fn apply_offered_option_tags(extra_headers: &mut Vec<MsgHeader>, offered: &[String]) {
    if offered.is_empty() {
        return;
    }
    let name = <header::kind::Supported as header::kind::HeaderKind>::name();
    let lines: Vec<TokenListHeader<header::kind::Supported>> = extra_headers
        .iter()
        .filter(|h| name.matches(&h.name))
        .filter_map(|h| TokenListHeader::parse(&h.value).ok())
        .collect();
    let stated = TokenListHeader::combine(lines).unwrap_or_else(TokenListHeader::empty);
    if offered.iter().all(|tag| stated.contains(tag)) {
        return;
    }
    let widened = offered.iter().fold(stated, |s, tag| s.with(tag.as_str()));
    extra_headers.retain(|h| !name.matches(&h.name));
    extra_headers.push(MsgHeader {
        name: SipStr::owned(name.as_wire_str()),
        value: SipStr::owned(&widened.to_wire()),
    });
}

/// The call-scoped withhold on an originated leg's assembled headers: the
/// `Supported` and `Require` sets are narrowed by `withheld`, and a set the
/// narrowing empties drops its header — a withheld tag leaves ONE wire form on
/// every leg the call originates, whichever mint assembled it. Lines not naming
/// a withheld tag are left byte-identical.
fn apply_withheld_option_tags(extra_headers: &mut Vec<MsgHeader>, withheld: &[String]) {
    if withheld.is_empty() {
        return;
    }
    narrow_token_set::<header::kind::Supported>(extra_headers, withheld);
    narrow_token_set::<header::kind::Require>(extra_headers, withheld);
}

/// Narrow one token-set header's lines by `withheld`, in place: no line, or
/// none naming a withheld tag, leaves the headers untouched; otherwise the
/// lines collapse to ONE restated line without the withheld tags (several
/// lines of a set-like header are one set, RFC 3261 §7.3.1), and a set left
/// empty drops its header — an emptied set claims nothing, which is what an
/// absent line already says (§20.37).
fn narrow_token_set<K: header::kind::TokenKind>(
    extra_headers: &mut Vec<MsgHeader>,
    withheld: &[String],
) {
    let name = K::name();
    let lines: Vec<TokenListHeader<K>> = extra_headers
        .iter()
        .filter(|h| name.matches(&h.name))
        .filter_map(|h| TokenListHeader::<K>::parse(&h.value).ok())
        .collect();
    let Some(set) = TokenListHeader::combine(lines) else {
        return;
    };
    if !withheld.iter().any(|tag| set.contains(tag)) {
        return;
    }
    let kept = withheld.iter().fold(set, |s, tag| s.without(tag));
    extra_headers.retain(|h| !name.matches(&h.name));
    if !kept.is_empty() {
        extra_headers.push(MsgHeader {
            name: SipStr::owned(name.as_wire_str()),
            value: SipStr::owned(&kept.to_wire()),
        });
    }
}

/// Clamp a decision-supplied ring deadline (s) for an originated leg under the
/// configured INVITE transaction bound (`B2buaConfig::clamp_no_answer_sec`),
/// with the per-call `debug!` note on a clamp — the ONE clamp site both
/// `NoAnswer` arming paths (`decision::apply_route`, `actions::create_leg`)
/// funnel through.
pub(crate) fn clamp_no_answer(config: &B2buaConfig, call_ref: &str, requested: i64) -> i64 {
    let clamped = config.clamp_no_answer_sec(requested);
    if clamped != requested {
        tracing::debug!(
            %call_ref,
            requested_sec = requested,
            clamped_sec = clamped,
            invite_txn_timeout_sec = config.invite_txn_timeout_sec,
            "no_answer_timeout clamped under the INVITE transaction bound"
        );
    }
    clamped
}

/// Build a fresh b-leg + its outbound INVITE effect (initial route + failover).
///
/// Errs when a decision-supplied address (`new_ruri` / `new_from` / `new_to`)
/// does not read: there is no b-leg to originate, and the caller answers the
/// affected leg instead of dialing a fabricated target.
#[allow(clippy::too_many_arguments)]
pub fn build_b_leg(
    call_ref: &str,
    leg_id: &str,
    // The call's emergency state (`call.emergency == Some(true)`); stamps the
    // `;em=1` / `;emerg=1` markers on the originated b-leg INVITE's Via +
    // Contact, so every subsequent in-dialog packet of the call stays
    // identifiable as emergency traffic on the wire.
    is_emergency: bool,
    a_leg_invite: &SipRequest,
    dest: (String, u16),
    new_ruri: Option<&str>,
    // Identity rewrites (ADR-0017): the b-leg From / To as a name-addr or a bare
    // addr-spec. The B2BUA owns the tags: a `tag` stated on either is dropped.
    // `None` keeps the relayed a-leg URI. The basic path passes `(None, None)`.
    new_from: Option<&str>,
    new_to: Option<&str>,
    no_answer_timeout_sec: Option<i64>,
    config: &B2buaConfig,
    id_gen: &IdGen,
    // REFER transfer overrides: `body_override` replaces the cloned a-leg body
    // (held SDP, or empty = drop); `header_updates` set/remove extra headers on
    // the transfer INVITE. The basic-B2BUA path passes `(None, &[])`.
    body_override: Option<&[u8]>,
    header_updates: &[(String, Option<String>)],
    // Capability set advertised on this originated leg
    // (`Allow`/`Supported`/`Accept`), resolved by the caller: declared, else
    // relayed from the originator, else no line (`rules::capabilities`). A
    // `header_updates` entry naming a half is more specific and wins.
    capabilities: &CapabilitySet,
    // RFC 7315 §5.6 charging correlation. `Some` stamps an icid identifying
    // this leg's charging session; `None` stamps none. A vector the originator
    // sent is relayed either way and never re-minted — re-minting it breaks the
    // correlation between the two operators' records.
    charging: Option<&call::features::ChargingVectorFeature>,
    // The option tags this call never offers a leg it originates
    // (`rules::capabilities::withheld_option_tags`): the call-scoped
    // declaration, latched across reroutes, and the armed strategy's own. The
    // assembled `Supported`/`Require` lines are narrowed by them, whatever
    // source stated them — the withhold is the call's incapability and
    // outranks every advertisement.
    withheld_option_tags: &[String],
    // The tags the call offers this leg on the stack's own behalf
    // (`rules::capabilities::offered_option_tags`): stated on the assembled
    // `Supported` line whatever source stated it, before the withhold, which
    // outranks it.
    offered_option_tags: &[String],
    // Leg role (ADR-0014/0016). `None` ⇒ [`LegKind::Destination`]. `adopted` is
    // left `None` so it derives from the kind (`is_adopted`): a `media` leg is
    // unadopted and thus gated out of the generic relay-to-peer fallback.
    kind: Option<call::LegKind>,
) -> Result<(Leg, OutboundSipEffect), UnreadableAddress> {
    let branch = id_gen.new_branch();
    let from_tag = id_gen.new_tag();
    let b_call_id = format!("{}-{}@{}", leg_id, id_gen.new_tag(), config.sip_local_ip);
    // Each rewrite is read here or the leg is not built. `None` keeps the
    // relayed a-leg value, which the parser already accepted.
    let request_uri = match new_ruri {
        Some(text) => address("new_ruri", text)?,
        None => a_leg_invite.request_uri().clone(),
    };
    let from_addr = match new_from {
        Some(text) => identity("new_from", text)?,
        None => NameAddr::new(a_leg_invite.from().uri().clone()),
    };
    let to_addr = match new_to {
        Some(text) => identity("new_to", text)?,
        None => NameAddr::new(a_leg_invite.to().uri().clone()),
    };
    let from_uri = from_addr.uri().clone();
    let to_uri = to_addr.uri().clone();
    let body = match body_override {
        Some(b) => b.to_vec(),
        None => a_leg_invite.body().to_vec(),
    };
    let content_type = if body.is_empty() {
        None
    } else {
        a_leg_invite
            .raw(HeaderName::ContentType)
            .next()
            .and_then(media_type)
            .or_else(|| body_override.map(|_| sdp()))
    };
    // `(name, Some(v))` sets, `(name, None)` removes — either way the name is the
    // caller's and no relayed or configured copy of it rides (see [`removed`]).
    // Neither reaches a STRUCTURAL name (RFC 3261 §16.6): the generator owns
    // those on the leg it mints, so a decision stating one would ride BESIDE the
    // stack's line rather than replace it — two `Max-Forwards`, and a budget the
    // decision could refill.
    let mut extra_headers: Vec<MsgHeader> = header_updates
        .iter()
        .filter(|(n, _)| HeaderName::class_of(n) != HeaderClass::Structural)
        .filter_map(|(n, v)| {
            v.as_ref().map(|val| MsgHeader { name: n.clone().into(), value: val.clone().into() })
        })
        .collect();
    // Advertise this face's capability set on the originated b-leg INVITE (RFC
    // 3261 §20.5/§20.37/§20.1) — the originator's own, relayed, unless the
    // call declares one; a half nobody stated carries no line. The offer and
    // the withhold narrow the assembled sets after this. Neither clobbers a
    // caller-supplied value from `header_updates`, and a half the caller
    // removed stays off (see [`removed`]).
    for (name, value) in capabilities.lines() {
        if !extra_headers.iter().any(|h| name.matches(&h.name)) && !removed(header_updates, &name) {
            extra_headers.push(MsgHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&value),
            });
        }
    }

    // Names the deployment states must ride, copied from the a-leg INVITE. A
    // name the relay withholds is refused here too, so configuration cannot
    // reach past the transparency rules; a value `header_updates` already set
    // stands (case-insensitive).
    for configured in &config.relay_headers {
        let name = HeaderName::from(configured.as_str());
        if extra_headers.iter().any(|h| name.matches(&h.name))
            || removed(header_updates, &name)
            || !generators::relayable(configured, relay_scope(body_override))
        {
            continue;
        }
        // Every line of the name rides: a set-like header the originator split
        // over several lines is one set (RFC 3261 §7.3.1).
        for v in a_leg_invite.raw_text(name.clone()) {
            extra_headers.push(MsgHeader { name: SipStr::owned(configured), value: v });
        }
    }

    // RFC 3261 §16.6: every header the originator sent that this stack does not
    // own rides onto the leg it originates — one mint point for BOTH originated
    // legs (the callee leg, and the REFER transfer leg whose `a_leg_invite` is
    // alice's rehydrated snapshot). A name stated above is the more specific
    // statement and stands: an explicit `header_updates` value, then this face's
    // advertisement, then the relayed value.
    let stated = extra_headers.clone();
    for header in generators::relayable_headers(a_leg_invite.headers(), relay_scope(body_override))
    {
        let name = HeaderName::from(header.name.as_str());
        if !stated.iter().any(|h| name.matches(&h.name)) && !removed(header_updates, &name) {
            extra_headers.push(header);
        }
    }

    // The call's own offer, then its withhold, on the assembled option-tag
    // sets — after every source has stated its lines, so no later mint of the
    // same names can resurface a withheld tag or lose an offered one; the
    // withhold runs last and outranks the offer.
    apply_offered_option_tags(&mut extra_headers, offered_option_tags);
    apply_withheld_option_tags(&mut extra_headers, withheld_option_tags);

    // RFC 7315 §5.6: the element that STARTS a leg generates the identifier its
    // charging session is correlated on. One already on the message — relayed
    // from the originator, or stated by the decision — is that identifier, so
    // this only ever mints where none arrived.
    if let Some(charging) = charging {
        let name = ChargingVector::header_name();
        if !extra_headers.iter().any(|h| name.matches(&h.name)) {
            let host = charging.generated_at.clone().unwrap_or_else(|| config.sip_local_ip.clone());
            let icid = format!("{}-{}", id_gen.new_tag(), leg_id);
            extra_headers.push(MsgHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&ChargingVector::new(icid, host).to_wire()),
            });
        }
    }

    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(request_uri.clone()),
        call_id: b_call_id.clone(),
        from: Some(header::From::new(from_addr).with_tag(SipStr::owned(&from_tag))),
        to: Some(header::To::new(to_addr)),
        cseq: 1,
        via: Some(leg_via(config, call_ref, leg_id, is_emergency, branch.clone())),
        contact: Some(leg_contact(config, call_ref, leg_id, is_emergency)),
        // §16.6 step 3: the originated leg continues the budget of the INVITE
        // that caused it rather than refilling — a B2BUA that restated 70 would
        // let a routing loop through it run forever. A REFER transfer leg reads
        // the rehydrated a-leg INVITE, so each turn of a REFER loop starts one
        // lower too.
        max_forwards: Some(hops::forwarded_max_forwards(a_leg_invite).value()),
        body,
        content_type,
        extra_headers,
    };
    let invite = generators::generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
    // Behind the front proxy, the b-leg INVITE traverses the proxy: preload a
    // plain loose Route and make the wire destination the proxy (R-URI stays the
    // callee). The proxy classifies the initial INVITE worker-outbound from the
    // top Via and double-record-routes the dialog. `wire_dest` drives the client
    // transaction's send + retransmits; the preloaded Route rides the snapshot so
    // retransmits carry it too.
    let (invite, wire_dest) = apply_b_leg_egress(config, leg_id, &[], invite, dest.clone());

    // The `call` crate stores dialog identity as text (ADR-0008), so the values
    // this INVITE was built from are written down as the bytes it carries.
    let from_uri = from_uri.text().into_owned();
    let to_uri = to_uri.text().into_owned();
    let request_uri = request_uri.text().into_owned();

    let dialog = Dialog {
        sip: StackDialog {
            call_id: b_call_id.clone(),
            local_tag: from_tag.clone(),
            remote_tag: String::new(),
            local_uri: from_uri.clone(),
            remote_uri: to_uri.clone(),
            remote_target: request_uri.clone(),
            local_cseq: 1,
            route_set: vec![],
        },
        ext: B2buaDialogExt {
            remote_cseq: None,
            inbound_pending_requests: vec![],
            ack_branch: None,
            pending_invite_txn: Some(InviteTxnHandle {
                branch: branch.clone(),
                original_invite: invite.image().to_vec(),
                destination: call::HostPort { host: wire_dest.0.clone(), port: wire_dest.1 },
            }),
            cached_sdp: None,
            pending_reinvite_2xx: None,
            answered_2xx: None,
            emitted_ack: None,
            awaited_ack_cseq: None,
        },
    };

    // Capture the INVITE handle before `dialog` is moved into the leg.
    let leg_invite_handle = dialog.ext.pending_invite_txn.clone();
    let leg = Leg {
        leg_id: leg_id.to_string(),
        call_id: b_call_id,
        from_tag,
        source: RemoteInfo { address: dest.0.clone(), port: dest.1 },
        state: LegState::Trying,
        disposition: LegDisposition::Pending,
        dialogs: vec![dialog],
        no_answer_timeout_sec,
        bye_disposition: None,
        local_uri: Some(from_uri),
        remote_uri: Some(to_uri),
        invite_request_uri: Some(request_uri),
        // Also stamp the INVITE handle on the leg: a forked early dialog created
        // from a later 18x has no per-dialog handle, so ACK-for-2xx / RAck CSeq
        // fall back to the leg's (RFC 3261 §13.2.2.4 / RFC 3262 §7.2).
        pending_invite_txn: leg_invite_handle,
        ext: None,
        kind: Some(kind.unwrap_or(call::LegKind::Destination)),
        // Derive adoption from the kind (don't pin it): Destination ⇒ adopted,
        // Media ⇒ unadopted. See `call::helpers::is_adopted`.
        adopted: None,
        invite_final_sent: None,
        messages: Default::default(),
    };

    let effect = OutboundSipEffect {
        body: OutboundBody::Request(invite),
        mode: OutboundTxnMode::NewClient(TxnKind::Invite),
        destination: wire_dest,
        label: format!("b-leg INVITE ({leg_id})"),
        leg_id: Some(leg_id.to_string()),
        // The originator's INVITE forwarded, unless the decision put its own
        // body on it — then the offer, and the message, are this stack's.
        provenance: match body_override {
            Some(_) => Provenance::Authored,
            None => Provenance::Relayed,
        },
    };
    Ok((leg, effect))
}
