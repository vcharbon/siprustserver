//! The two sides of a transaction, named by the sockets its REQUEST used.
//!
//! A party may send from one socket and listen on another: a UAC sends from
//! an ephemeral port and names its listening socket as the Via sent-by, which
//! its responses reach (RFC 3261 §18.2.2); a UAS takes requests on one socket
//! and answers from another. A rule that pairs a request with the responses of
//! its own transaction therefore files a response under the socket its
//! request used on the same side, never under the response's own socket.

use std::collections::BTreeMap;

use crate::rules::branch::BranchName;
use crate::rules::via::sender_listens_at;
use crate::wire::{Kind, Msg};

/// The request each response of a view answers.
///
/// The request is the one of the same Call-ID, top-Via branch and CSeq method
/// (§17.1.3). A key carried between two different endpoint pairs names no
/// request: a relay that keeps the branch shows two transactions under one
/// key. A repeat and a message with no Call-ID or branch name nothing.
pub(crate) struct Sides<'m> {
    requests: BTreeMap<TransactionKey<'m>, Option<&'m Msg>>,
}

impl<'m> Sides<'m> {
    /// The requests of `msgs`, a view's stream in observation order.
    pub(crate) fn of(msgs: impl IntoIterator<Item = &'m Msg>) -> Self {
        let mut requests: BTreeMap<TransactionKey<'m>, Option<&'m Msg>> = BTreeMap::new();
        for msg in msgs.into_iter().filter(|m| !m.repeat && !m.call_id.is_empty()) {
            if !matches!(msg.kind, Kind::Request { .. }) {
                continue;
            }
            let Some(key) = transaction_key(msg) else { continue };
            requests
                .entry(key)
                .and_modify(|seen| {
                    if seen.is_some_and(|r| (&r.src, &r.dst) != (&msg.src, &msg.dst)) {
                        *seen = None;
                    }
                })
                .or_insert(Some(msg));
        }
        Sides { requests }
    }

    /// The request `response` answers, `None` where the view names none. A
    /// response between the request's own endpoints in the request's own
    /// direction answers nothing.
    pub(crate) fn request_of(&self, response: &Msg) -> Option<&'m Msg> {
        if !matches!(response.kind, Kind::Response { .. }) || response.call_id.is_empty() {
            return None;
        }
        let request = self.requests.get(&transaction_key(response)?).copied().flatten()?;
        ((&request.src, &request.dst) != (&response.src, &response.dst)).then_some(request)
    }

    /// The socket `msg`'s UAC side sent its request from: a request's source;
    /// a response's request's source where the response reached an address
    /// that request's sender wrote (`sender_listens_at`), else the response's
    /// destination.
    pub(crate) fn uac<'a>(&self, msg: &'a Msg) -> &'a str
    where
        'm: 'a,
    {
        match self.request_of(msg) {
            Some(request) if sender_listens_at(request, &msg.dst) => request.src.as_str(),
            _ if matches!(msg.kind, Kind::Request { .. }) => msg.src.as_str(),
            _ => msg.dst.as_str(),
        }
    }

    /// The socket `msg`'s UAS side took the request at: a request's
    /// destination; a response's request's destination, else the response's
    /// source.
    pub(crate) fn uas<'a>(&self, msg: &'a Msg) -> &'a str
    where
        'm: 'a,
    {
        match self.request_of(msg) {
            Some(request) => request.dst.as_str(),
            None if matches!(msg.kind, Kind::Request { .. }) => msg.dst.as_str(),
            None => msg.src.as_str(),
        }
    }
}

/// `(Call-ID, top-Via branch, CSeq method)`: the transaction a message belongs
/// to (§17.1.3).
type TransactionKey<'m> = (&'m str, BranchName<'m>, String);

/// The transaction `msg` belongs to, `None` without a branch.
fn transaction_key(msg: &Msg) -> Option<TransactionKey<'_>> {
    let branch = BranchName(msg.via_branch.as_deref().filter(|b| !b.is_empty())?);
    Some((msg.call_id.as_str(), branch, msg.cseq_method.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests;
