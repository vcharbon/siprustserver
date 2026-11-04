//! [`RelayOwner`] — the worker that relayed a request, named in the request's
//! record so a final finds the worker whose client transaction carries it
//! (RFC 3261 §18.2.2).

use serde::{Deserialize, Serialize};

/// A worker named by its ordinal and its worker identity, one boot of that
/// ordinal. Two owners are the same worker only when both are equal: a worker
/// restarted under the same ordinal is another owner (ADR-0055 §6). The default
/// names no worker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelayOwner {
    pub ordinal: String,
    pub identity: String,
}

impl RelayOwner {
    pub fn new(ordinal: impl Into<String>, identity: impl Into<String>) -> Self {
        Self { ordinal: ordinal.into(), identity: identity.into() }
    }
}
