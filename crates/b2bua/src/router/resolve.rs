//! Event → call resolution: derive the `callRef`, source leg, direction and
//! call incarnation for every event kind (synchronously), including the
//! acting-backup takeover re-key from the replica store's SIP index.

use call::{Call, Direction, IndexHit, IndexLookup, KeyKind};
use sip_message::header::{ParamValue, Via};
use sip_message::{Method, SipMessage};

use super::materialise::{names_server_branch, names_unanswered_caller_invite};
use super::topology_cookie::foreign_primary;
use super::undecided::is_undecided;
use super::RouterCtx;
use crate::store::StoreError;
use b2bua_sdk::event::CallEvent;

/// How an event resolves to a call + the leg it arrived on.
pub(super) struct Resolution {
    pub(super) call_ref: Option<String>,
    pub(super) source_leg_id: String,
    pub(super) direction: Direction,
    pub(super) initial_invite: bool,
    /// The incarnation of the call the event belongs to: the one a timer, a
    /// timeout or an internal event was stamped with, or the `callRef` with
    /// the `ci` mark our Via (a response) or Contact (an in-dialog request's
    /// Request-URI) carried. `None` when it states none: it belongs to
    /// whichever call is live on the `callRef`.
    pub(super) incarnation: Option<String>,
    /// The namespace of the SIP index key the call was found by, when the
    /// event states no leg of its own: `source_leg_id` is then a placeholder
    /// until [`bind_indexed_leg`](Self::bind_indexed_leg) names the leg that
    /// owns the key.
    pub(super) indexed: Option<KeyKind>,
}

impl Resolution {
    /// Take the call an index lookup found for an event that states no
    /// `callRef`; an event that states no leg either is bound to the leg
    /// owning the matched key once the call is in hand.
    pub(super) fn found_by_index(&mut self, hit: IndexHit, event: &CallEvent) {
        self.call_ref = Some(hit.call_ref);
        if !states_leg(event) {
            self.indexed = Some(hit.kind);
        }
    }

    /// Take the replicated call an initial INVITE is a copy of
    /// ([`initial_invite_copy`]): the INVITE is that call's, never a new
    /// call's.
    pub(super) fn copy_of(&mut self, hit: IndexHit, event: &CallEvent) {
        self.found_by_index(hit, event);
        self.initial_invite = false;
    }

    /// Bind the source leg of an event whose call the SIP index found to the
    /// leg of `call` that owns the matched key ([`IndexLookup::owning_leg`]).
    /// A resolution the event stated a leg for, or whose owning leg `call`
    /// does not hold, is left as it is.
    pub(super) fn bind_indexed_leg(&mut self, call: &Call, event: &CallEvent) {
        let Some(kind) = self.indexed else { return };
        let Some(lookup) = event_lookup(event) else { return };
        if let Some(leg) = lookup.owning_leg(kind, call) {
            self.source_leg_id = leg.to_string();
            self.direction = leg_direction(leg);
        }
    }
}

/// Resolve the `callRef`, source leg and incarnation for an event
/// (synchronous, no blocking).
pub(super) fn resolve(ctx: &RouterCtx, event: &CallEvent) -> Resolution {
    let mut res = resolve_call(ctx, event);
    res.incarnation = incarnation(event, res.call_ref.as_deref());
    res
}

/// The incarnation `event` states for the call `call_ref` names
/// ([`Resolution::incarnation`]).
fn incarnation(event: &CallEvent, call_ref: Option<&str>) -> Option<String> {
    let CallEvent::Sip { message, .. } = event else {
        return event.incarnation().map(str::to_string);
    };
    // Read beside our own `callRef` / `cr` only: a Request-URI that is not
    // our Contact states no incarnation of ours.
    let mark = match message.as_ref() {
        SipMessage::Request(req) => {
            let uri = req.request_uri();
            uri.param("callRef").and(uri.param("ci"))
        }
        SipMessage::Response(resp) => resp.top_via().param("cr").and(resp.top_via().param("ci")),
    };
    let mark = mark.and_then(ParamValue::as_str).map(crate::stack_identity::decode_param)?;
    Some(call::derive_incarnation(call_ref?, &mark))
}

fn resolve_call(ctx: &RouterCtx, event: &CallEvent) -> Resolution {
    match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => {
                if req.method() == Method::Invite && req.to().tag().is_none() {
                    let call_ref = call::derive_call_ref(
                        &ctx.config.self_ordinal,
                        req.call_id().as_str(),
                        req.from().tag().unwrap_or(""),
                    );
                    return Resolution {
                        call_ref: Some(call_ref),
                        source_leg_id: "a".into(),
                        direction: Direction::FromA,
                        initial_invite: true,
                        incarnation: None,
                        indexed: None,
                    };
                }
                // In-dialog request: read our cr/lg from the Request-URI params.
                // URI parameter names are case-insensitive (RFC 3261 §19.1.1), so
                // the stamped `callRef` answers to any spelling. A request that
                // states no `callRef` is found through the SIP index, the
                // in-memory one here and the replicated one on an acting
                // backup (`replica_takeover`). A CANCEL the transaction layer
                // passes up names an incoming INVITE (see `Cancelled` below).
                let mut res = Resolution {
                    direction: Direction::FromA,
                    call_ref: None,
                    source_leg_id: "a".into(),
                    initial_invite: false,
                    incarnation: None,
                    indexed: None,
                };
                if let Some(leg) = request_leg(req) {
                    res.direction = leg_direction(&leg);
                    res.source_leg_id = leg;
                }
                let stated = req
                    .request_uri()
                    .param("callRef")
                    .and_then(ParamValue::as_str)
                    .map(crate::stack_identity::decode_param);
                match stated {
                    Some(call_ref) => res.call_ref = Some(call_ref),
                    None => {
                        if let Some(hit) = ctx.state.resolve_from_sip_key_sync(request_lookup(req))
                        {
                            res.found_by_index(hit, event);
                        }
                    }
                }
                res
            }
            SipMessage::Response(resp) => {
                // Response: read our cr/lg from the top Via we stamped.
                let ids = via_cr_lg(resp.top_via()).unwrap_or(ViaIds { cr: None, lg: "a".into() });
                let mut res = Resolution {
                    direction: leg_direction(&ids.lg),
                    call_ref: ids.cr,
                    source_leg_id: ids.lg,
                    initial_invite: false,
                    incarnation: None,
                    indexed: None,
                };
                if res.call_ref.is_none() {
                    if let Some(hit) = ctx.state.resolve_from_sip_key_sync(response_lookup(resp)) {
                        res.found_by_index(hit, event);
                    }
                }
                res
            }
        },
        CallEvent::Cancelled { call_id, from_tag, .. } => {
            // The index names the call when it holds the cancelled INVITE's
            // identity (see `IndexLookup::Cancel` for which keys qualify). On a
            // miss the callRef is derived as the INVITE derived it, so a CANCEL
            // that overtakes its INVITE's `create()` still queues behind that
            // INVITE on the same call; one with no INVITE reaches no live call
            // and takes the orphan path in `process`.
            let call_ref = ctx
                .state
                .resolve_from_sip_key_sync(IndexLookup::Cancel { call_id, from_tag })
                .map(|hit| hit.call_ref)
                .unwrap_or_else(|| {
                    call::derive_call_ref(&ctx.config.self_ordinal, call_id, from_tag)
                });
            Resolution {
                call_ref: Some(call_ref),
                source_leg_id: "a".into(),
                direction: Direction::FromA,
                initial_invite: false,
                incarnation: None,
                indexed: None,
            }
        }
        CallEvent::Timeout { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(&leg),
                call_ref: call_ref.clone(),
                source_leg_id: leg,
                initial_invite: false,
                incarnation: None,
                indexed: None,
            }
        }
        CallEvent::Timer { call_ref, key, .. } => {
            let leg = key.leg_id().unwrap_or("a").to_string();
            Resolution {
                direction: leg_direction(&leg),
                call_ref: Some(call_ref.clone()),
                source_leg_id: leg,
                initial_invite: false,
                incarnation: None,
                indexed: None,
            }
        }
        CallEvent::InternalEvent { call_ref, .. } => Resolution {
            call_ref: Some(call_ref.clone()),
            source_leg_id: "a".into(),
            direction: Direction::FromA,
            initial_invite: false,
            incarnation: None,
            indexed: None,
        },
        // A lifecycle event is delivered, never dispatched: it names no call.
        CallEvent::Lifecycle(_) => Resolution {
            call_ref: None,
            source_leg_id: "a".into(),
            direction: Direction::FromA,
            initial_invite: false,
            incarnation: None,
            indexed: None,
        },
    }
}

/// Recover the takeover call of an in-dialog SIP request from the replica
/// store's SIP index (the acting-backup production path). In-dialog requests
/// (those carrying a To-tag) and a CANCEL — which names its INVITE's Call-ID
/// and From-tag (RFC 3261 §9.1), the same key, for a ringing call a peer
/// admitted — are candidates; an initial request, a response, or a non-SIP
/// event is never a dialog takeover. `Ok(None)` when not applicable or no
/// replica matches — the caller then treats the event as unroutable; `Err`
/// when the replica read failed.
pub(super) async fn replica_takeover(
    ctx: &RouterCtx,
    event: &CallEvent,
) -> Result<Option<IndexHit>, StoreError> {
    let CallEvent::Sip { message, .. } = event else { return Ok(None) };
    let SipMessage::Request(req) = message.as_ref() else { return Ok(None) };
    if req.to().tag().is_none() && req.method() != Method::Cancel {
        return Ok(None);
    }
    ctx.state.resolve_from_replica_index(request_lookup(req)).await
}

/// What an initial INVITE is to this node ([`initial_invite_copy`]).
pub(super) enum InitialInvite {
    /// A call of this node's own, new or a copy of one here.
    Own,
    /// A copy of the replicated call the hit names: taken over, it is absorbed
    /// by the transaction the record rebuilds for it. `unanswered`: it copies
    /// the caller's unanswered initial INVITE, whose transaction the record
    /// rebuilds whole, so the layer's transaction for the copy gives way to it.
    CopyOf { hit: IndexHit, unanswered: bool },
    /// Another primary's INVITE that copies no transaction of a record this
    /// node holds: the primary owns its transaction, and placing it would
    /// dial a second call.
    Foreign,
}

/// **A copy of a replicated call's initial INVITE.** An INVITE with no To-tag,
/// with nothing here under its own `callRef`, reaching a node that
/// replicates, is a copy (RFC 3261 §17.2.3) of the replicated call whose
/// incoming leg it names when it carries that call's INVITE CSeq and its
/// top-Via branch names that call's own server transaction
/// ([`names_server_branch`]), answered or not — the caller retransmitting to
/// the backup of a dead primary, through the cookie or through a fresh
/// selection — whatever worker the cookie names: the node holding the record
/// of a decided call serves it. Failing that, an INVITE whose cookie names
/// another primary is that primary's (the proxy re-placed its
/// retransmission): foreign. Any other is this node's own, as is every
/// INVITE of a node that does not replicate: it backs up no primary.
pub(super) async fn initial_invite_copy(
    ctx: &RouterCtx,
    event: &CallEvent,
    res: &Resolution,
) -> InitialInvite {
    let (true, Some(derived)) = (res.initial_invite, res.call_ref.as_deref()) else {
        return InitialInvite::Own;
    };
    let CallEvent::Sip { message, .. } = event else { return InitialInvite::Own };
    let SipMessage::Request(req) = message.as_ref() else { return InitialInvite::Own };
    let cseq = req.cseq().seq();
    if !ctx.state.replicates() || ctx.unborn.holds(derived, cseq) || ctx.state.contains(derived) {
        return InitialInvite::Own;
    }
    if let Some((hit, unanswered)) = replicated_call_copied(ctx, req).await {
        ctx.state.note_takeover_resolved(&hit);
        return InitialInvite::CopyOf { hit, unanswered };
    }
    match foreign_primary(req, &ctx.config.self_ordinal) {
        Some(_) => InitialInvite::Foreign,
        None => InitialInvite::Own,
    }
}

/// The replicated call `req` is a copy of the initial INVITE of: its
/// incoming leg is `req`'s, its INVITE CSeq `req`'s, and `req`'s top-Via
/// branch names its own server transaction; with whether that transaction is
/// the caller's unanswered initial INVITE. An undecided call is no copy's: its
/// decision may still be in flight at a live primary; the proxy's dead
/// verdict, the departure or a reclaim takes it over and asks its decision
/// again (ADR-0047 D4).
async fn replicated_call_copied(
    ctx: &RouterCtx,
    req: &sip_message::SipRequest,
) -> Option<(IndexHit, bool)> {
    let call_id = req.call_id().as_str();
    let from_tag = req.from().tag().unwrap_or("");
    let lookup = IndexLookup::Invite { call_id, from_tag };
    let hit = ctx.state.replica_index_hit(lookup).await.ok()??;
    let (copy, _) = ctx.state.peek_replica(&hit.call_ref).await.ok()?;
    let call = copy.call;
    let branch = req.top_via().branch().unwrap_or_default();
    let copied = call.a_leg_invite.cseq == req.cseq().seq() && names_server_branch(&call, branch);
    (copied && !is_undecided(&call)).then(|| (hit, names_unanswered_caller_invite(&call, branch)))
}

/// The index lookup for a request that carries no `callRef` of ours: a CANCEL
/// names the incoming INVITE it cancels, any other request its sender's
/// dialog tag.
fn request_lookup(req: &sip_message::SipRequest) -> IndexLookup<'_> {
    let call_id = req.call_id().as_str();
    let from_tag = req.from().tag().unwrap_or("");
    if req.method() == Method::Cancel {
        IndexLookup::Cancel { call_id, from_tag }
    } else {
        IndexLookup::Peer { call_id, tag: from_tag }
    }
}

/// The index lookup for a response that carries no `callRef` of ours: the
/// tag its sender chose.
fn response_lookup(resp: &sip_message::SipResponse) -> IndexLookup<'_> {
    IndexLookup::Peer { call_id: resp.call_id().as_str(), tag: resp.to().tag().unwrap_or("") }
}

/// The index lookup a SIP event resolves by when it states no `callRef`.
fn event_lookup(event: &CallEvent) -> Option<IndexLookup<'_>> {
    let CallEvent::Sip { message, .. } = event else { return None };
    Some(match message.as_ref() {
        SipMessage::Request(req) => request_lookup(req),
        SipMessage::Response(resp) => response_lookup(resp),
    })
}

/// The leg our Contact stamped on a request's Request-URI.
fn request_leg(req: &sip_message::SipRequest) -> Option<String> {
    req.request_uri()
        .param("leg")
        .and_then(ParamValue::as_str)
        .map(crate::stack_identity::decode_param)
}

/// Whether a SIP event names the leg it arrived on: the Request-URI `leg`
/// our Contact stamped, or the `lg` our Via stamped.
fn states_leg(event: &CallEvent) -> bool {
    let CallEvent::Sip { message, .. } = event else { return false };
    match message.as_ref() {
        SipMessage::Request(req) => request_leg(req).is_some(),
        SipMessage::Response(resp) => resp.top_via().param("lg").is_some(),
    }
}

fn leg_direction(leg: &str) -> Direction {
    if leg == "a" {
        Direction::FromA
    } else {
        Direction::FromB
    }
}

/// The `;cr=`/`;lg=` identity params stamped on a Via we emitted.
struct ViaIds {
    cr: Option<String>,
    lg: String,
}

/// Extract the [`ViaIds`] from a Via header value's `;cr=`/`;lg=` params.
fn via_cr_lg(via: &Via) -> Option<ViaIds> {
    let cr = via.param("cr").and_then(ParamValue::as_str);
    let lg = via.param("lg").and_then(ParamValue::as_str);
    if cr.is_none() && lg.is_none() {
        return None;
    }
    Some(ViaIds {
        cr: cr.map(crate::stack_identity::decode_param),
        lg: lg.map(crate::stack_identity::decode_param).unwrap_or_else(|| "a".into()),
    })
}
