//! The session descriptions exchanged on one leg's dialog, as far as the
//! stack keeps them ([`LegSdpSession`]): enough for a description from
//! another author to continue the session the leg's peer holds (RFC 3264 §8),
//! and for the peer's own descriptions to go back in that author's order.

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
    /// The sess-id of the last description the leg's peer sent.
    pub received_session_id: Option<String>,
}
