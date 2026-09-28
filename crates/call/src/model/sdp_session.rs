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
