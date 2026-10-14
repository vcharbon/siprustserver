//! The HA topology a new call is born under, read off the front proxy's
//! stickiness cookie.

use call::CallTopology;
use sip_message::header::{ParamValue, RecordRouteEntry};
use sip_message::SipRequest;

/// Derive the HA [`CallTopology`] from the proxy's stickiness cookie, carried as
/// URI params on the front proxy's Record-Route (`w_pri`/`w_bak`/`e`/`v`/`kid`/
/// `sig`; see `sip-proxy` `load_balancer.rs` / `build_record_route_value`). The
/// b2bua reads `w_pri`/`w_bak` so the backup peer AGREES with the proxy's
/// rendezvous (HRW 2nd-best) choice by construction rather than re-deriving it
/// (the proxy keys HRW off the alive-set + Call-ID the b2bua cannot reproduce
/// locally — see [`crate::repl::replication_target`]).
///
/// The proxy double-record-routes (`ProxyCore` §16.6): it inserts BOTH a cookie
/// RR and a direction-only `;outbound` RR, and on an inbound INVITE the
/// `;outbound` half is topmost. So we scan all Record-Route headers for the one
/// that actually carries the stickiness grammar rather than assuming the first.
///
/// - Cookie present → [`CookieRead::Topology`] carrying `CallTopology { pri:
///   w_pri (or self_ordinal if the param is absent/empty), bak: w_bak (may be
///   empty), gen: 1 }`. A brand-new call starts at `gen = 1`; the b2bua's update
///   path bumps it per mutation (see [`crate::store::CallState::update`]).
/// - Every recorded route reads and none carries the stickiness grammar
///   (non-proxied / legacy INVITE) → [`CookieRead::NoCookie`].
/// - A recorded route no reader accepts → [`CookieRead::Unreadable`]. The cookie
///   may well be there; this read cannot see it, which is NOT the same fact as
///   "no proxy recorded one". The caller decides — and must say so out loud.
///
/// The read is all-or-nothing: one unreadable recorded route makes the whole
/// list unreadable, including a cookie half that would have read on its own.
pub(super) fn read_topology_cookie(invite: &SipRequest, self_ordinal: &str) -> CookieRead {
    // Find the Record-Route that carries the stickiness cookie; the partner
    // `;outbound` half (direction only) has no `w_pri`/`w_bak`.
    let recorded = match invite.list::<RecordRouteEntry>() {
        Ok(recorded) => recorded,
        Err(err) => return CookieRead::Unreadable(err),
    };
    let Some(cookie) = recorded
        .iter()
        .map(|entry| entry.uri())
        .find(|uri| uri.param("w_pri").is_some() || uri.param("w_bak").is_some())
    else {
        return CookieRead::NoCookie;
    };
    let pri = cookie
        .param("w_pri")
        .and_then(ParamValue::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(self_ordinal)
        .to_string();
    let bak = cookie.param("w_bak").and_then(ParamValue::as_str).unwrap_or("").to_string();
    // Brand-new call: primary counter p = 1 (the primary "created" it), backup
    // counter b = 0 (no takeover yet). See `CallTopology` / ADR-0014.
    CookieRead::Topology(CallTopology::new(pri, bak, 1, 0))
}

/// What the inbound INVITE's Record-Route says about the proxy's stickiness
/// cookie. Two of the three outcomes are ordinary; the third is a placement the
/// call loses, so they are kept apart rather than collapsed to `Option`.
#[derive(Debug)]
pub(super) enum CookieRead {
    /// The cookie reads: the call's HA topology comes off it.
    Topology(CallTopology),
    /// The recorded routes read and carry no stickiness cookie — a non-proxied
    /// or legacy INVITE. Non-replicating placement is correct here.
    NoCookie,
    /// A recorded route no reader accepts, so the cookie — if any — is
    /// invisible.
    Unreadable(sip_message::SipParseError),
}

/// The call's HA topology, with the unreadable read named on stderr. A recorded
/// route no reader accepts falls back to no topology — the flush path then stays
/// non-replicating (`PutOpts::default()`), same as a genuinely cookie-less
/// INVITE — because inventing `pri`/`bak` from a header we cannot read would
/// have the backup peer disagree with the proxy's rendezvous choice, which is
/// worse than not replicating. The fallback is silent-degrade-shaped, so it is
/// never silent: the call is placed on this worker only and does not survive a
/// takeover.
pub(super) fn topology_from_cookie(
    invite: &SipRequest,
    self_ordinal: &str,
    call_ref: &str,
) -> Option<CallTopology> {
    match read_topology_cookie(invite, self_ordinal) {
        CookieRead::Topology(topology) => Some(topology),
        CookieRead::NoCookie => None,
        CookieRead::Unreadable(err) => {
            tracing::warn!(
                %call_ref,
                call_id = invite.call_id().as_str(),
                error = %err,
                "a recorded route does not read; the proxy's stickiness cookie cannot be read, so \
                 this call is placed NON-REPLICATING — it does not survive a takeover"
            );
            None
        }
    }
}
