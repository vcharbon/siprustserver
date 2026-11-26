//! [`Presentation`]: how an initial INVITE writes what it carries, apart from
//! what it carries.

use sip_message::header::Wire;

use crate::agent::Invite;

/// The display names an initial INVITE states and the wire spelling of the
/// headers the stack writes. A display name is raw text: the writer quotes it
/// and escapes `"`, `\` and controls (RFC 3261 §25.1 `quoted-pair`). A name
/// form `(canonical, wire)` spells every header named `canonical` as `wire` on
/// the datagram only; the transaction keeps the canonical spelling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Presentation {
    pub from_display: Option<String>,
    pub to_display: Option<String>,
    pub name_forms: Vec<(String, String)>,
}

impl Presentation {
    /// The bytes this presentation adds to the INVITE: each display name as
    /// the writer quotes and escapes it, and the space after it.
    pub fn display_bytes(&self) -> usize {
        let displays = [&self.from_display, &self.to_display].into_iter().flatten();
        displays.map(|d| quoted_len(d) + 1).sum()
    }

    /// `inv` stating these display names and wire names.
    pub fn apply<'b>(&self, mut inv: Invite<'b>) -> Invite<'b> {
        if let Some(display) = &self.from_display {
            inv = inv.from_display(display);
        }
        if let Some(display) = &self.to_display {
            inv = inv.to_display(display);
        }
        inv.with_name_forms(&self.name_forms)
    }
}

/// The bytes `display` takes once the writer quotes and escapes it.
pub fn quoted_len(display: &str) -> usize {
    let mut wire = Wire::new();
    wire.quoted(display);
    wire.as_str().len()
}
