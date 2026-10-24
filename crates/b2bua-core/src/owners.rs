//! [`Owners`]: who holds the call, as the shell states it for one turn
//! (ADR-0052 §2). The call record holds none of it (ADR-0055 §11): the turn
//! reads it as a plain input.

use call::RelayOwner;

/// The owners of the call a turn runs on, as the shell read them at the
/// turn's start. The default names no node and no other owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owners {
    /// This node, as a request record names the node that relayed it.
    pub this: RelayOwner,
    /// The owner of the call other than this node, stated on every Via the
    /// turn emits; `None` for a call with one owner.
    pub other: Option<String>,
    /// Whether another owner served the call, so its record may lack what
    /// that owner sent (ADR-0055 §4).
    pub copy_served: bool,
}

impl Owners {
    /// No node and no other owner, for a turn no shell states owners for.
    pub fn none() -> &'static Owners {
        static NONE: Owners = Owners {
            this: RelayOwner { ordinal: String::new(), identity: String::new() },
            other: None,
            copy_served: false,
        };
        &NONE
    }
}
