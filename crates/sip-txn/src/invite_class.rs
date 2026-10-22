//! The class a new INVITE is judged in by every admission stage that refuses
//! one, read off the parsed request so each stage sees the dialog and the
//! priority the parser sees.

use sip_message::emergency::is_emergency_request;
use sip_message::SipRequest;

/// The class an INVITE is judged in: the `class` of the refusal counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InviteClass {
    /// A new initial INVITE.
    Normal,
    /// A new initial INVITE carrying an emergency priority (RFC 4412).
    Emergency,
    /// An INVITE carrying a To-tag.
    InDialog,
}

impl InviteClass {
    /// Every class, in exposition order.
    pub const ALL: [InviteClass; 3] = [Self::Normal, Self::Emergency, Self::InDialog];

    /// Stable metric label (`class="..."`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Emergency => "emergency",
            Self::InDialog => "in_dialog",
        }
    }

    /// The class's position in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// The class of `req`: in a dialog when its To carries a tag (RFC 3261
/// §12.2), else emergency when it carries an emergency priority (RFC 4412),
/// else normal.
pub fn class_of(req: &SipRequest) -> InviteClass {
    if req.to().tag().is_some() {
        InviteClass::InDialog
    } else if is_emergency_request(req) {
        InviteClass::Emergency
    } else {
        InviteClass::Normal
    }
}
