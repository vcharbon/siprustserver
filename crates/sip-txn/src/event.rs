//! Upward events the transaction layer emits to its consumer (the proxy's or
//! B2BUA's router), and the client-transaction handles `send_request` returns.
//! Port of the `TransactionEvent` / `ClientTransactionHandle` types in
//! `TransactionLayer.ts`.

use std::net::SocketAddr;

use sip_message::SipMessage;

/// A deduplicated/processed event for the upstream router.
///
/// Note: unlike the source, there is no `rinfo` arm carrying a JS `RemoteInfo`
/// — the peer address is a `SocketAddr` (sip-net's everywhere-`SocketAddr`
/// convention, ADR-0005).
#[derive(Debug, Clone)]
pub enum TransactionEvent {
    /// A SIP message that survived dedup/absorption and should reach the app.
    Message {
        message: Box<SipMessage>,
        src: SocketAddr,
    },
    /// A CANCEL matched a server INVITE txn; the 200/487 were sent by this
    /// layer and the call should be torn down upstream.
    Cancelled { call_id: String, from_tag: String },
    /// A client transaction's Timer B/F fired with no final response — the
    /// transaction timed out.
    Timeout {
        branch: String,
        call_ref: Option<String>,
        leg_id: Option<String>,
        /// SIP method of the timed-out transaction (INVITE / BYE / …).
        method: Option<String>,
    },
}

/// Reason class for an event shed when the bounded output queue is full.
/// Operators want to know *which* message class is dropped under backpressure,
/// not just an aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventQueueDropReason {
    RequestInvite,
    RequestOther,
    Response,
    Cancelled,
    Timeout,
}

impl EventQueueDropReason {
    pub(crate) fn of(event: &TransactionEvent) -> Self {
        match event {
            TransactionEvent::Cancelled { .. } => Self::Cancelled,
            TransactionEvent::Timeout { .. } => Self::Timeout,
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Response(_) => Self::Response,
                SipMessage::Request(r) if r.method == "INVITE" => Self::RequestInvite,
                SipMessage::Request(_) => Self::RequestOther,
            },
        }
    }

    pub(crate) const ALL: [EventQueueDropReason; 5] = [
        Self::RequestInvite,
        Self::RequestOther,
        Self::Response,
        Self::Cancelled,
        Self::Timeout,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::RequestInvite => 0,
            Self::RequestOther => 1,
            Self::Response => 2,
            Self::Cancelled => 3,
            Self::Timeout => 4,
        }
    }
}

/// INVITE vs non-INVITE — the two transaction state machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnKind {
    Invite,
    NonInvite,
}

use sip_message::SipRequest;

/// Handle to an outgoing client transaction, returned by `send_request`. Later
/// messages sourced from the same transaction (CANCEL, ACK-for-2xx) reuse the
/// branch / original request RFC 3261 mandates (§9.1, §13.2.2.4).
#[derive(Debug, Clone)]
pub enum ClientTransactionHandle {
    Invite {
        branch: String,
        original_invite: SipRequest,
        destination: SocketAddr,
    },
    NonInvite {
        branch: String,
        original_request: SipRequest,
        destination: SocketAddr,
    },
}

impl ClientTransactionHandle {
    pub fn branch(&self) -> &str {
        match self {
            Self::Invite { branch, .. } | Self::NonInvite { branch, .. } => branch,
        }
    }
    pub fn destination(&self) -> SocketAddr {
        match self {
            Self::Invite { destination, .. } | Self::NonInvite { destination, .. } => *destination,
        }
    }
}
