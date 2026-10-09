//! What a b-leg keeps about its fork stragglers (RFC 3261 §13.2.2.4): the
//! local sequence each losing early dialog had spent when another fork's 2xx
//! confirmed the leg, and the release this stack sent a straggler that later
//! answered — the branch of its ACK, re-sent on every repeat of that 2xx, and
//! the branch of its BYE, whose final and timeout belong to the release alone
//! and never to the winning dialog. Kept in the leg's ext slot.

use std::collections::BTreeMap;

use call::Leg;
use serde::{Deserialize, Serialize};

/// The leg ext slot the book lives in.
pub const SLOT: &str = "fork-straggler";

/// One straggler this stack released.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The branch of the ACK that answered its 2xx.
    pub ack_branch: String,
    /// The branch of the BYE that ends its dialog.
    pub bye_branch: String,
}

/// A leg's fork-straggler bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    /// Losing early dialog's To-tag → the last local CSeq it spent.
    #[serde(default)]
    pub forks: BTreeMap<String, i64>,
    /// Released straggler's To-tag → its release.
    #[serde(default)]
    pub released: BTreeMap<String, Release>,
}

impl Book {
    /// The book `leg` keeps; empty where it keeps none.
    pub fn of(leg: &Leg) -> Book {
        leg.ext
            .as_ref()
            .and_then(|ext| ext.get(SLOT))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Write this book back into `leg`'s ext slot.
    pub fn store(&self, leg: &mut Leg) {
        let value = serde_json::to_value(self).unwrap_or_default();
        leg.ext.get_or_insert_with(Default::default).insert(SLOT.to_string(), value);
    }

    /// Whether the BYE a final with To-tag `tag` answers is a straggler release.
    pub fn releases(&self, tag: &str) -> bool {
        self.released.contains_key(tag)
    }

    /// Whether the client transaction on `branch` is a straggler's BYE.
    pub fn owns_bye_branch(&self, branch: &str) -> bool {
        self.released.values().any(|r| r.bye_branch == branch)
    }
}
