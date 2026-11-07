//! The seeds a meeting of two copies hands the transaction layer: what the node
//! taking the other copy can serve of its rounds (ADR-0055 §8).

use call::Call;

use super::seeds::seeds_for;

/// The seeds a meeting with the other copy hands the layer: the INVITE server
/// transactions answered 2xx whose ACK the met record still awaits, in RFC
/// 6026 Accepted with the 2xx it retains. They are the only transactions of
/// another node's rounds this node can serve while that node lives: a copy of
/// the INVITE reaches whichever node the proxy reads alive, and the 2xx that
/// answers it is in the record; the rest of a round runs where its Via sends
/// the target's responses, the node that relayed it.
pub(in crate::router) fn answered_invite_seeds(call: &Call) -> Vec<sip_txn::TxnSeed> {
    // No instant moves an INVITE seed: the latest one is read.
    seeds_for(call, i64::MAX)
        .into_iter()
        .filter(|s| matches!(s, sip_txn::TxnSeed::ServerAccepted { .. }))
        .collect()
}

/// The seeds the call's primary hands the layer when it takes its backup's
/// copy at a meeting, for the legs that copy dialled and `live`, the
/// primary's own copy until the meeting, does not: each leg's dialling INVITE
/// another node sent that has no final, as a client INVITE. A leg's cookie
/// names the call's primary, so the INVITE's responses reach it (ADR-0055
/// §8), and it takes the INVITE's client transaction over from the record
/// (ADR-0047 D7).
pub(in crate::router) fn dialled_invite_seeds(
    call: &Call,
    live: &Call,
    this: &call::RelayOwner,
) -> Vec<sip_txn::TxnSeed> {
    let branches: Vec<&str> = call
        .b_legs
        .iter()
        .filter(|leg| leg.invite.as_ref().is_some_and(|r| r.relayed_by != *this))
        .filter(|leg| leg.outbound_invite().is_some_and(|i| i.final_received.is_none()))
        .filter_map(|leg| leg.dialled_sent())
        .map(|sent| sent.branch.as_str())
        .filter(|branch| !dials_on(live, branch))
        .collect();
    // No instant moves an INVITE seed: the latest one is read.
    seeds_for(call, i64::MAX)
        .into_iter()
        .filter(|s| match s {
            sip_txn::TxnSeed::ClientInvite { invite, .. } => {
                invite.top_via().branch().is_some_and(|b| branches.contains(&b))
            }
            _ => false,
        })
        .collect()
}

/// Whether `call` holds a leg whose dialling INVITE left on `branch`.
fn dials_on(call: &Call, branch: &str) -> bool {
    call.b_legs.iter().filter_map(|leg| leg.dialled_sent()).any(|sent| sent.branch == branch)
}
