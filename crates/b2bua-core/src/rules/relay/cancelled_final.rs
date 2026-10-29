//! The 487 the transaction layer answers a CANCEL of the caller's initial
//! INVITE with (RFC 3261 §9.2), as the record retains it: the layer holds it
//! for the turn that takes the `Cancelled` event, so a node serving the call
//! from that turn's write sends it when the node that held it is gone.

use call::{Call, Repeated, RetainedEmission};
use sip_message::generators::{self, GenerateResponseOpts};

use crate::turn_notes::TurnNotes;

/// The 487 to `call`'s caller INVITE under `to_tag`, the tag the layer
/// answered under, retained to be sent to her INVITE's response target.
/// `None` when the record does not rebuild her INVITE.
pub fn cancelled_final(
    call: &Call,
    to_tag: Option<String>,
    notes: &mut TurnNotes,
) -> Option<RetainedEmission> {
    let invite = super::caller_invite(call, notes)?;
    let opts = GenerateResponseOpts { to_tag, ..Default::default() };
    let resp = generators::generate_response(&invite, 487, "Request Terminated", &opts).ok()?;
    let (host, port) = invite.top_via().response_target();
    let repeated = Repeated::response("INVITE", 487);
    Some(RetainedEmission::on_trigger(resp.image().to_vec(), (host.to_string(), port), repeated))
}
