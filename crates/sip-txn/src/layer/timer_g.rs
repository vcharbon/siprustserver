//! RFC 3261 §17.2.1 Timer G: a server INVITE transaction's non-2xx final
//! re-sent on its ladder while the transaction waits in `Completed` for the
//! ACK.

use call::TxnKind;
use sip_net::UdpEndpoint;

use super::key::ServerTxnId;
use super::owner::Owner;
use super::txn::{Timer, TxnState};

impl Owner {
    /// RFC 3261 §17.2.1 Timer G: retransmit the cached non-2xx final of an INVITE
    /// server txn still in `Completed` (not yet ACKed / Timer-H'd), then re-arm at
    /// MIN(2×interval, T2). The auto-100 we already sent silenced the UAC's INVITE
    /// retransmit, so the passive "replay the cached final on a request retransmit"
    /// path never fires — without this a single dropped reject wedges the caller
    /// for the full 32 s (Timer H). The ACK (`hold_for_timer_i`) cancels
    /// `retransmit_key` and Timer H deletes the transaction, so this stops
    /// exactly when RFC requires.
    /// Non-INVITE (Timer J) and 2xx (TU-owned §13.3.1.4 retransmit) are excluded at
    /// the arming site in `do_send_response`.
    pub(super) async fn fire_server_retransmit(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        id: ServerTxnId<'_>,
    ) {
        let (buf, dest, status, call_id) = match self.server(id) {
            Some(t) if t.kind == TxnKind::Invite && t.state == TxnState::Completed => {
                match (&t.last_response, t.destination, t.last_response_status) {
                    (Some(buf), Some(dest), Some(status)) => {
                        (buf.clone(), dest, status, t.call_id.clone())
                    }
                    _ => return, // no cached final / destination — nothing to resend
                }
            }
            _ => return, // ACKed (Confirmed), Timer-H'd, or no longer a completed INVITE server txn
        };

        self.send_buffer(endpoint, &call_id, &buf, dest).await;
        self.metrics.retransmits.record_final(status);

        let rearm = match self.server_mut(id).and_then(|t| t.ladder.as_mut()) {
            Some(ladder) => ladder.advance(),
            None => return,
        };
        if let Some(next_interval) = rearm {
            let key = self.arm(Timer::ServerRetransmit(id.to_key()), next_interval);
            if let Some(t) = self.server_mut(id) {
                t.retransmit_key = Some(key);
            }
        }
    }
}
