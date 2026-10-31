//! What a confirmed caller keeps of its establishing INVITE: the seed of the
//! dialog a losing fork's late 2xx confirms (RFC 3261 §13.2.2.4), and the
//! derivation of that fork's [`Dialog`] from it.

use std::collections::HashMap;
use std::net::SocketAddr;

use sip_message::generators::StackDialog;
use sip_message::SipResponse;

use super::super::addressing::next_hop;
use super::super::dialog::Dialog;
use super::super::Agent;
use super::{first_contact_uri, uac_route_set, ClientInvite};

/// The establishing INVITE once its 2xx is ACKed. The INVITE client
/// transaction is over (§17.1.1.2), so neither the request nor its datagram
/// is kept: a repeat of the winner's 2xx is re-ACKed from the confirmed
/// dialog, and a losing fork's late 2xx needs only the dialog state at the
/// ACK, the per-fork CSeq map and the INVITE's CSeq number.
pub(crate) struct WonInvite {
    invite_cseq: u32,
    fallback_addr: SocketAddr,
    dialog: StackDialog,
    fork_cseq: HashMap<String, u32>,
}

impl ClientInvite {
    /// The establishing INVITE reduced to what its confirmed caller keeps.
    pub(crate) fn into_won(self) -> WonInvite {
        WonInvite {
            invite_cseq: self.invite_cseq(),
            fallback_addr: self.fallback_addr,
            dialog: self.dialog,
            fork_cseq: self.fork_cseq,
        }
    }
}

impl WonInvite {
    /// The establishing INVITE's CSeq number, which a fork's 2xx echoes.
    pub(crate) fn invite_cseq(&self) -> u32 {
        self.invite_cseq
    }

    /// The confirmed [`Dialog`] a fork's 2xx establishes, `agent` sending on
    /// it (see [`ClientInvite::fork_dialog`]).
    pub(crate) fn fork_dialog(&self, agent: &Agent, resp: &SipResponse) -> Dialog {
        fork_dialog(agent, &self.dialog, &self.fork_cseq, self.fallback_addr, resp)
    }
}

/// The dialog a fork's 2xx `resp` confirms, derived from the INVITE's dialog
/// state: the response's own To-tag and route set, empty when it records none,
/// and its Contact as the remote target (RFC 3261 §12.1.2; the INVITE's target
/// when it carries none), the CSeq continuing from the fork's own PRACK
/// sequence when it had one so its BYE stays contiguous within its dialog
/// (§12.2.1.1).
pub(super) fn fork_dialog(
    agent: &Agent,
    invite_dialog: &StackDialog,
    fork_cseq: &HashMap<String, u32>,
    fallback_addr: SocketAddr,
    resp: &SipResponse,
) -> Dialog {
    let mut dialog = invite_dialog.clone();
    if let Some(tag) = &resp.to().tag() {
        dialog.remote_tag = tag.to_string();
    }
    if let Some(target) = first_contact_uri(resp) {
        dialog.remote_target = target;
    }
    dialog.route_set = uac_route_set(resp);
    if let Some(&fork) = fork_cseq.get(&dialog.remote_tag) {
        dialog.local_cseq = dialog.local_cseq.max(fork);
    }
    let dst = next_hop(&dialog, fallback_addr);
    Dialog::new(agent.clone(), dst, dialog)
}
