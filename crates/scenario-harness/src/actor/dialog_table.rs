//! The dialogs an actor's endpoint owns.

use crate::{ClientInvite, Dialog};

/// The confirmed dialog(s) + pending INVITE transaction an endpoint owns. Deliberately minimal: one
/// caller INVITE, one confirmed dialog — enough for every current body incl. the realign flows,
/// which ride the confirmed dialog as in-dialog UAS transactions. (The reliable-183 / `Reject`
/// pending-UAS holds live as [`ActorState`] fields, not here.)
#[derive(Default)]
pub(super) struct DialogTable {
    /// The caller's outgoing INVITE, awaiting its confirmation (learned from the
    /// responses the reactor feeds it via [`ClientInvite::absorb_response`]).
    pub(super) pending_invite: Option<Box<ClientInvite>>,
    /// Our confirmed dialog (caller after ACK, or UAS after answering).
    pub(super) confirmed: Option<Dialog>,
    /// The caller's establishing INVITE, RETAINED after confirmation (C1/E3):
    /// a LOSING fork's late 2xx (§13.2.2.4) arrives after the winner's and must
    /// be ACK+BYE'd on ITS OWN fork dialog — derived from this transaction
    /// ([`ClientInvite::fork_dialog`]), never from the confirmed (winner) one.
    pub(super) won_invite: Option<Box<ClientInvite>>,
}
