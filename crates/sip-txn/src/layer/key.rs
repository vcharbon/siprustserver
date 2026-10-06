//! The identity of a server transaction (RFC 3261 §17.2.3): the top-Via
//! branch, the top-Via sent-by and the method, an ACK naming its INVITE's
//! transaction. Two requests on one branch from two sent-bys, or with two
//! methods, are two transactions. A client transaction is its branch alone
//! (§17.1.3: its responses match on branch and CSeq method).
//!
//! [`ServerTxnKey`] is the owned key the server map files a transaction
//! under; [`ServerTxnId`] is the same identity borrowed from a message, so the
//! per-datagram lookup allocates nothing.

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};

use sip_message::header::{SentBy, SentByRef, Via};
use sip_message::{Method, SipRequest, SipResponse};

/// The method a server transaction is filed under for `method`: an ACK's is
/// its INVITE's.
fn filed_method(method: &Method) -> &Method {
    static INVITE: Method = Method::Invite;
    match method {
        Method::Ack => &INVITE,
        m => m,
    }
}

/// A server transaction's identity, owned (§17.2.3).
#[derive(Debug, Clone)]
pub struct ServerTxnKey {
    branch: String,
    sent_by: SentBy,
    method: Method,
}

impl ServerTxnKey {
    /// The server transaction `req` opens or belongs to; `None` when its top
    /// Via carries no branch.
    pub fn of(req: &SipRequest) -> Option<Self> {
        ServerTxnId::of_request(req).map(ServerTxnId::to_key)
    }

    pub(super) fn new(branch: String, sent_by: SentBy, method: &Method) -> Self {
        Self { branch, sent_by, method: filed_method(method).clone() }
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub(super) fn id(&self) -> ServerTxnId<'_> {
        ServerTxnId {
            branch: &self.branch,
            sent_by: self.sent_by.as_borrowed(),
            method: &self.method,
        }
    }
}

/// A server transaction's identity, borrowed from a message.
#[derive(Debug, Clone, Copy)]
pub struct ServerTxnId<'a> {
    branch: &'a str,
    sent_by: SentByRef<'a>,
    method: &'a Method,
}

impl<'a> ServerTxnId<'a> {
    /// The identity a message with top Via `via` and (CSeq) method `method`
    /// names; `None` without a branch.
    pub(super) fn new(via: &'a Via, method: &'a Method) -> Option<Self> {
        let branch = via.branch().filter(|b| !b.is_empty())?;
        Some(Self { branch, sent_by: via.sent_by_ref(), method: filed_method(method) })
    }

    pub(super) fn of_request(req: &'a SipRequest) -> Option<Self> {
        Self::new(req.top_via(), req.method())
    }

    /// The transaction a response belongs to: it echoes its request's top Via
    /// and names its method in CSeq.
    pub(super) fn of_response(resp: &'a SipResponse) -> Option<Self> {
        Self::new(resp.top_via(), resp.cseq().method())
    }

    /// The INVITE transaction a CANCEL with top Via `via` names (§9.2).
    pub(super) fn cancelled_invite(via: &'a Via) -> Option<Self> {
        static INVITE: Method = Method::Invite;
        Self::new(via, &INVITE)
    }

    pub(super) fn branch(&self) -> &'a str {
        self.branch
    }

    pub(super) fn sent_by(&self) -> SentByRef<'a> {
        self.sent_by
    }

    pub(super) fn to_key(self) -> ServerTxnKey {
        ServerTxnKey {
            branch: self.branch.to_string(),
            sent_by: self.sent_by.to_sent_by(),
            method: self.method.clone(),
        }
    }
}

impl PartialEq for ServerTxnId<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.branch == other.branch && self.method == other.method && self.sent_by == other.sent_by
    }
}

impl Hash for ServerTxnId<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.branch.hash(state);
        self.sent_by.hash(state);
        self.method.hash(state);
    }
}

/// What the server map and index are looked up by: an owned key or a
/// borrowed identity, hashed and compared as one ([`ServerTxnId`]).
pub trait ServerTxnIdentity {
    #[doc(hidden)]
    fn identity(&self) -> ServerTxnId<'_>;
}

impl ServerTxnIdentity for ServerTxnKey {
    fn identity(&self) -> ServerTxnId<'_> {
        self.id()
    }
}

impl ServerTxnIdentity for ServerTxnId<'_> {
    fn identity(&self) -> ServerTxnId<'_> {
        *self
    }
}

impl PartialEq for dyn ServerTxnIdentity + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for dyn ServerTxnIdentity + '_ {}

impl Hash for dyn ServerTxnIdentity + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

impl<'a> Borrow<dyn ServerTxnIdentity + 'a> for ServerTxnKey {
    fn borrow(&self) -> &(dyn ServerTxnIdentity + 'a) {
        self
    }
}

impl PartialEq for ServerTxnKey {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for ServerTxnKey {}

impl Hash for ServerTxnKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}
