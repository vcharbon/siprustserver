//! The two keys the CANCEL family files a transaction under: by its UAS side
//! and by its UAC side, each named by the socket its requests used
//! ([`Sides`]). A branch in either compares ignoring case (RFC 3261 §7.3.1).

use crate::rules::branch::BranchName;
use crate::rules::sides::Sides;
use crate::wire::Msg;

/// One INVITE transaction as ONE endpoint saw it: the call it rides, the
/// top-Via branch that names it, and which endpoint is its UAS side. Spelled
/// out as a struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TxnKey<'a> {
    /// The endpoint judged: the UAS that TOOK the INVITE and emits its answers.
    pub(super) emitter: &'a str,
    pub(super) call_id: &'a str,
    pub(super) branch: BranchName<'a>,
}

impl<'a> TxnKey<'a> {
    /// The transaction `msg`'s RECIPIENT is the UAS side of — an INVITE or its
    /// CANCEL arriving on `branch`.
    pub(super) fn taken_by(msg: &'a Msg, branch: &'a str) -> Self {
        TxnKey {
            emitter: msg.dst.as_str(),
            call_id: msg.call_id.as_str(),
            branch: BranchName(branch),
        }
    }

    /// The transaction `msg`'s SENDER is the UAS side of — a response going out
    /// on `branch`, filed under the socket its request reached.
    pub(super) fn emitted_by(sides: &Sides<'a>, msg: &'a Msg, branch: &'a str) -> Self {
        TxnKey {
            emitter: sides.uas(msg),
            call_id: msg.call_id.as_str(),
            branch: BranchName(branch),
        }
    }
}

/// One INVITE transaction as ONE endpoint DROVE it: the call it rides, the
/// top-Via branch that names it, and which endpoint is its UAC side. The mirror
/// of [`TxnKey`], which names the same transaction by its UAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct UacKey<'a> {
    /// The endpoint judged: the UAC that SENT the INVITE and its CANCEL.
    pub(super) emitter: &'a str,
    pub(super) call_id: &'a str,
    pub(super) branch: BranchName<'a>,
}

impl<'a> UacKey<'a> {
    /// The transaction `msg`'s SENDER drives — a request going out on `branch`.
    pub(super) fn sent_by(msg: &'a Msg, branch: &'a str) -> Self {
        UacKey {
            emitter: msg.src.as_str(),
            call_id: msg.call_id.as_str(),
            branch: BranchName(branch),
        }
    }

    /// The transaction `msg`'s RECIPIENT drives — a response coming back on
    /// `branch`, filed under the socket its request left from.
    pub(super) fn taken_by(sides: &Sides<'a>, msg: &'a Msg, branch: &'a str) -> Self {
        UacKey {
            emitter: sides.uac(msg),
            call_id: msg.call_id.as_str(),
            branch: BranchName(branch),
        }
    }
}
