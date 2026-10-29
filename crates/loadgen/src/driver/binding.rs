//! A call's agent layout on the mux: its routing, its agent binds, its
//! from-user correlation key, and the outcome of a refused bind.

use e2e_model::LegSpec;
use scenario_harness::{Agent, AgentBinder};

use super::{MuxTransport, CALLER};
use crate::class::CallOutcome;
use crate::mux::{labelled_prefix_leg_picker_defaulting, CallRouting};

/// The per-call routing the driver declares: one correlation token per call,
/// the callee legs on the callee address, and the R-URI picker when there are
/// several.
///
/// Two-tier callee demux. The token is the FIRST tier — it selects the call
/// INSTANCE (mux `by_token`); the SUT carries it onto every downstream leg, so
/// every callee leg shares it. Every callee-side leg (the shape's named
/// `LegSpec`s — by default bob, the rerouting `bob2`, the transfer
/// `charlie`) shares ONE socket (`transport.uas_addr`); the SECOND tier —
/// WHICH leg of this instance — is the R-URI-prefix leg picker, fed each leg's
/// declared `ruri_prefixes` labelled with its role. The in-house shapes' legs
/// arrive under their role (`sip:bob2@…`, `sip:charlie@…`: the reroute plan's
/// `new_ruri`, the transfer's Refer-To user); an open-registry shape's legs
/// arrive under number-plan digits (`+1555…`, `9007…`) that its specs map back
/// to the role. Either way no leg needs a per-leg socket.
pub(super) fn call_routing(transport: &MuxTransport, legs: &[LegSpec], token: &str) -> CallRouting {
    let mut routing = CallRouting::new(token);
    // Shared-socket layout: the caller binds the callee legs' address, so
    // every bind there is declared, the caller first (the bind order below).
    if transport.uac_addr == transport.uas_addr {
        routing = routing.caller(transport.uac_addr);
    }
    for leg in legs {
        routing = routing.leg(transport.uas_addr, leg.role);
    }
    if legs.len() > 1 {
        // The SUT originates its PRIMARY callee leg from its own route config, so
        // that INVITE's R-URI is the bare route target (`sip:host:port`, userless)
        // — unlike a transfer (Refer-To → `sip:charlie@…`) or reroute
        // (`new_ruri` → `sip:bob2@…`) leg, which carries a distinguishing
        // user-part. Route such a userless leg to the primary (first-declared)
        // role instead of orphaning it; otherwise every multi-leg shape (refer,
        // reroute) drops its very first bob leg as `no endpoint to route to`.
        routing = routing.picker(
            transport.uas_addr,
            labelled_prefix_leg_picker_defaulting(
                legs.iter().flat_map(|leg| leg.ruri_prefixes.iter().map(|p| (*p, leg.role))),
                Some(legs[0].role),
            ),
        );
    }
    routing
}

/// Bind the call's agents: alice, then the callee legs in `legs` order (the
/// order the routing declares them — load-bearing on a shared socket). A
/// refusal is returned as is (see [`bind_refusal`]); the agents bound so far
/// deregister as they drop.
pub(super) async fn bind_agents(
    binder: &AgentBinder,
    transport: &MuxTransport,
    legs: &[LegSpec],
) -> Result<(Agent, Vec<(&'static str, Agent)>), sip_net::BindError> {
    let alice = binder.try_agent(CALLER, transport.uac_addr).await?;
    let mut callees = Vec::with_capacity(legs.len());
    for leg in legs {
        callees.push((leg.role, binder.try_agent(leg.role, transport.uas_addr).await?));
    }
    Ok((alice, callees))
}

/// The from-user correlation key of a call: the URI user of its resolved
/// caller identity, or the bounded rejection reason when there is none.
pub(super) fn from_user_key(from: Option<&str>) -> Result<String, &'static str> {
    let from = from.ok_or("no_from")?;
    sip_message::sniff::address_user(from).ok_or("userless_from")
}

/// The outcome of a refused agent bind: a key a concurrent call holds is a
/// `key_in_flight` rejection, a key cooling after a failed call a
/// `key_cooling` one; any other refusal (no mux endpoint at the address, more
/// binds than declared) is a layout defect, a counted panic.
pub(super) fn bind_refusal(e: &sip_net::BindError) -> CallOutcome {
    match e.reason {
        sip_net::BindErrorReason::AlreadyBound => CallOutcome::Rejected("key_in_flight"),
        sip_net::BindErrorReason::HeldBack => CallOutcome::Rejected("key_cooling"),
        _ => CallOutcome::Panic(format!("agent bind refused: {e}")),
    }
}
