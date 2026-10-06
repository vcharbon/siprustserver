//! The form a session description leaves in: a hook a service supplies so the
//! stack can write a description it forwards as the service's deployment
//! requires, instead of the author's bytes. The stack's own default leaves
//! every description as written.

use std::fmt::Debug;

/// How a description leaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SdpForm {
    /// The author's bytes, apart from what a restatement changes (RFC 3264 §8).
    #[default]
    AsWritten,
    /// Re-serialized by [`sip_message::canonical_form`].
    Canonical,
}

/// What the stack knows of an in-dialog description it is about to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdpCrossing {
    /// The description is a peer's, relayed; `false` for one the stack wrote.
    pub by_peer: bool,
    /// The stack restates this description under the dialog's session.
    pub restated: bool,
    /// The stack has restated a description anywhere in the call, this one
    /// included.
    pub call_restated: bool,
}

/// Decides the form of an in-dialog description the stack sends. An opening
/// description (the initial INVITE's exchange) always leaves as written.
pub trait SdpFormPolicy: Debug + Send + Sync {
    fn form(&self, crossing: &SdpCrossing) -> SdpForm;
}

/// Every description as written: the stack's default.
#[derive(Clone, Copy, Debug, Default)]
pub struct AsWritten;

impl SdpFormPolicy for AsWritten {
    fn form(&self, _: &SdpCrossing) -> SdpForm {
        SdpForm::AsWritten
    }
}
