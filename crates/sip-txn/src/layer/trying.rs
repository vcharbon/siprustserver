//! The first provisional of a server INVITE transaction (RFC 3261 §17.2.1):
//! the transaction stays `Trying` with nothing sent until its consumer
//! releases the 100 ([`TransactionLayer::release_trying`]), or sends a
//! response of its own, and sends the 100 itself `trying_bound_ms` after its
//! bound is armed with neither. The bound is armed as the transaction opens,
//! or, for an initial INVITE under `consumer_arms_initial_trying`, when the
//! consumer arms it ([`TransactionLayer::arm_trying`], ADR-0070). A 100 that
//! leaves after the INVITE's receipt states the delay in its `Timestamp`
//! (§8.2.6.1).
//!
//! [`TransactionLayer::release_trying`]: crate::TransactionLayer::release_trying
//! [`TransactionLayer::arm_trying`]: crate::TransactionLayer::arm_trying

use sip_clock::{deadline, delay_queue};
use sip_message::generators::{delayed_trying_timestamp, generate_response, GenerateResponseOpts};
use sip_message::SipRequest;
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use call::TxnKind;

use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{Timer, TxnState};
use crate::timers::ms;

impl Owner {
    /// Whether the server transaction the INVITE `invite` opens arms its
    /// §17.2.1 bound as it opens: an in-dialog INVITE always, an initial one
    /// unless its consumer arms it.
    pub(super) fn trying_bound_at_opening(&self, invite: &SipRequest) -> bool {
        !self.consumer_arms_initial_trying || invite.to().tag().is_some()
    }

    /// Arm the §17.2.1 bound of the server INVITE transaction `key` opened at
    /// `opened`: the [`Timer::Trying`] that sends its 100 if nothing else has
    /// left by then.
    pub(super) fn arm_trying_bound(&mut self, key: ServerTxnKey, opened: Instant) {
        let at = deadline::after(opened, ms(self.trying_bound_ms));
        delay_queue::insert_at(&mut self.timers, Timer::Trying(key, opened), at);
    }

    /// The consumer armed the bound of the server INVITE transaction `key`
    /// names: its 100 leaves `trying_bound_ms` from now unless a response
    /// left first. A transaction gone, or one past `Trying`, arms nothing.
    pub(super) fn arm_trying_now(&mut self, key: ServerTxnKey) {
        let owed = self
            .server(key.id())
            .filter(|t| t.kind == TxnKind::Invite && t.state == TxnState::Trying)
            .map(|t| t.created_at);
        let Some(opened) = owed else { return };
        let at = deadline::after(Instant::now(), ms(self.trying_bound_ms));
        delay_queue::insert_at(&mut self.timers, Timer::Trying(key, opened), at);
    }

    /// The bound of the transaction `key` opened at `opened` passed: its 100
    /// leaves unless a response already left. A transaction opened since on the
    /// same key keeps its own bound.
    pub(super) async fn fire_trying(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        key: &ServerTxnKey,
        opened: Instant,
    ) {
        let same = self.server(key.id()).is_some_and(|t| t.created_at == opened);
        if same {
            self.send_first_trying(endpoint, key.id()).await;
        }
    }

    /// Send the 100 of the server INVITE transaction `id` names if it has sent
    /// nothing: still `Trying`, or seeded `Proceeding` with no provisional
    /// cached. It then stands `Proceeding` with the 100 cached for a
    /// retransmitted INVITE (§17.2.1). `true` when the 100 left.
    pub(super) async fn send_first_trying(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        id: ServerTxnId<'_>,
    ) -> bool {
        let Some(txn) = self.server(id) else { return false };
        let silent = match txn.state {
            TxnState::Trying => true,
            TxnState::Proceeding => txn.last_response.is_none(),
            _ => false,
        };
        let owed = txn.kind == TxnKind::Invite && silent;
        let (Some(request), Some(dest)) = (txn.original_request.as_deref(), txn.destination) else {
            return false;
        };
        if !owed {
            return false;
        }
        // A 100 that leaves as the INVITE is received echoes its Timestamp as
        // it is; a later one states the delay.
        let delay = txn.created_at.elapsed();
        let delayed = (delay.as_millis() > 0).then(|| delayed_trying_timestamp(request, delay));
        let opts = GenerateResponseOpts {
            extra_headers: delayed.flatten().into_iter().collect(),
            ..GenerateResponseOpts::default()
        };
        // An INVITE whose lines no response can echo draws no 100.
        let Ok(trying) = generate_response(request, 100, "Trying", &opts) else { return false };
        let image = trying.image().clone();
        self.send_buffer(endpoint, trying.call_id().as_str(), &image, dest).await;
        if let Some(txn) = self.server_mut(id) {
            txn.state = TxnState::Proceeding;
            txn.last_response = Some(image);
            txn.last_response_status = Some(100);
        }
        true
    }
}
