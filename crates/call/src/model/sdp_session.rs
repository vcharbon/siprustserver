//! The session descriptions exchanged on one leg's dialog, as far as the
//! stack keeps them ([`LegSdpSession`]): whose session the dialog carries,
//! what the stack last stated in it, and enough for a description from
//! another author to continue that session (RFC 3264 §8) and for the peer's
//! own descriptions to go back in that author's stream order.

use serde::{Deserialize, Serialize};

/// A leg's session state. Empty until the first description crosses the leg.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegSdpSession {
    /// The `o=` value of the last description this stack sent on the leg.
    pub sent_origin: Option<String>,
    /// The `m=` values of that description, in order.
    pub sent_media: Vec<String>,
    /// Where that description restated another author's: for each of its
    /// m-lines, the position of the author's stream it carries (`None` for a
    /// slot kept rejected). Empty when it left as its author wrote it.
    pub sent_slots: Vec<Option<u32>>,
    /// The leg whose peer wrote the description `sent_slots` restated: only a
    /// description going back to that leg is put in its stream order.
    pub sent_slots_author: Option<String>,
    /// For that restatement: the count of the peer's offers at the time and
    /// the `o=` value of the description it was made from. The same author's
    /// same version with no newer peer offer is a repeat, restated at the same
    /// version.
    pub restated_from: Option<String>,
    /// How many in-dialog offers the leg's peer has sent (an INVITE or UPDATE
    /// carrying a description), counted as each is received once the call
    /// exists — the a-leg's initial INVITE is never counted. An offer the rules
    /// then refuse (491, 500) counts too: its only effect is that the next
    /// unchanged description is a new version, which RFC 3264 §8 permits.
    #[serde(default)]
    pub offers_received: u32,
    /// The leg whose peer's own session the dialog carries; `None` when it is
    /// a session this stack opened (or none yet).
    pub session_author: Option<String>,
    /// That session's sess-id, as its author states it.
    pub session_id: Option<String>,
    /// This stack has stated a version of that session on its own account (a
    /// restatement): the author's own next description no longer continues
    /// the versions the peer holds and is restated too.
    pub restated: bool,
}
