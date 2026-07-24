//! Admissible-method views over [`Method`](crate::method::Method) — which
//! methods a generator may build in / out of dialog — plus the capability
//! sets the B2BUA advertises (Allow / Supported).

// RFC 3261 §13.2.1 / §20.37 — accepted methods + supported extensions, advertised
// on every B2BUA-originated INVITE so the peer can negotiate.
pub const B2BUA_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, REFER, NOTIFY, PRACK";
pub const B2BUA_SUPPORTED: &str = "100rel, timer, replaces";

/// Methods the in-dialog generator accepts (ACK excluded — it has its own
/// primitive [`generate_ack_for_2xx`](super::generate_ack_for_2xx), a
/// compile-time guarantee).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InDialogMethod {
    Bye,
    Invite,
    Prack,
    Notify,
    Options,
    Info,
    Update,
    Message,
    /// REFER (RFC 3515). The B2BUA never *originates* a REFER; this exists for
    /// the harness to send one from an agent (Refer-To / Replaces ride via
    /// `extra_headers`).
    Refer,
}

impl InDialogMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            InDialogMethod::Bye => "BYE",
            InDialogMethod::Invite => "INVITE",
            InDialogMethod::Prack => "PRACK",
            InDialogMethod::Notify => "NOTIFY",
            InDialogMethod::Options => "OPTIONS",
            InDialogMethod::Info => "INFO",
            InDialogMethod::Update => "UPDATE",
            InDialogMethod::Message => "MESSAGE",
            InDialogMethod::Refer => "REFER",
        }
    }
}

impl From<InDialogMethod> for crate::method::Method {
    fn from(m: InDialogMethod) -> Self {
        use crate::method::Method;
        match m {
            InDialogMethod::Bye => Method::Bye,
            InDialogMethod::Invite => Method::Invite,
            InDialogMethod::Prack => Method::Prack,
            InDialogMethod::Notify => Method::Notify,
            InDialogMethod::Options => Method::Options,
            InDialogMethod::Info => Method::Info,
            InDialogMethod::Update => Method::Update,
            InDialogMethod::Message => Method::Message,
            InDialogMethod::Refer => Method::Refer,
        }
    }
}

/// `InDialogMethod` is a *view* over [`Method`](crate::method::Method): exactly
/// the methods the B2BUA may send as an ordinary in-dialog request. `ACK`/`CANCEL`
/// (own primitives) and out-of-dialog-only methods (`REGISTER`/`SUBSCRIBE`/…) are
/// rejected — this is the admissibility invariant the generators rely on.
impl TryFrom<&crate::method::Method> for InDialogMethod {
    type Error = ();
    fn try_from(m: &crate::method::Method) -> Result<Self, Self::Error> {
        use crate::method::Method;
        Ok(match m {
            Method::Bye => InDialogMethod::Bye,
            Method::Invite => InDialogMethod::Invite,
            Method::Prack => InDialogMethod::Prack,
            Method::Notify => InDialogMethod::Notify,
            Method::Options => InDialogMethod::Options,
            Method::Info => InDialogMethod::Info,
            Method::Update => InDialogMethod::Update,
            Method::Message => InDialogMethod::Message,
            Method::Refer => InDialogMethod::Refer,
            _ => return Err(()),
        })
    }
}

/// Methods the out-of-dialog generator accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfDialogMethod {
    Invite,
    Options,
    Message,
    Register,
    Subscribe,
    Publish,
}

impl OutOfDialogMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutOfDialogMethod::Invite => "INVITE",
            OutOfDialogMethod::Options => "OPTIONS",
            OutOfDialogMethod::Message => "MESSAGE",
            OutOfDialogMethod::Register => "REGISTER",
            OutOfDialogMethod::Subscribe => "SUBSCRIBE",
            OutOfDialogMethod::Publish => "PUBLISH",
        }
    }
}

impl From<OutOfDialogMethod> for crate::method::Method {
    fn from(m: OutOfDialogMethod) -> Self {
        use crate::method::Method;
        match m {
            OutOfDialogMethod::Invite => Method::Invite,
            OutOfDialogMethod::Options => Method::Options,
            OutOfDialogMethod::Message => Method::Message,
            OutOfDialogMethod::Register => Method::Register,
            OutOfDialogMethod::Subscribe => Method::Subscribe,
            OutOfDialogMethod::Publish => Method::Publish,
        }
    }
}

/// `OutOfDialogMethod` is a *view* over [`Method`](crate::method::Method): the
/// methods the stack may originate outside a dialog.
impl TryFrom<&crate::method::Method> for OutOfDialogMethod {
    type Error = ();
    fn try_from(m: &crate::method::Method) -> Result<Self, Self::Error> {
        use crate::method::Method;
        Ok(match m {
            Method::Invite => OutOfDialogMethod::Invite,
            Method::Options => OutOfDialogMethod::Options,
            Method::Message => OutOfDialogMethod::Message,
            Method::Register => OutOfDialogMethod::Register,
            Method::Subscribe => OutOfDialogMethod::Subscribe,
            Method::Publish => OutOfDialogMethod::Publish,
            _ => return Err(()),
        })
    }
}
