//! A confirmed caller keeps its establishing INVITE for the whole call, so
//! every byte of what it keeps is held per call: the seed of the dialog a
//! losing fork's late 2xx confirms (RFC 3261 §13.2.2.4), never the INVITE
//! request or the datagram it went out as.

use std::collections::HashMap;
use std::mem::size_of;
use std::net::SocketAddr;

use sip_message::generators::StackDialog;
use sip_message::SipRequest;

use crate::actor::dialog_table::DialogTable;

/// The size of the value an `Option<Box<T>>` slot holds once filled.
fn boxed_size<T>(_: &Option<Box<T>>) -> usize {
    size_of::<T>()
}

/// What a losing fork's dialog is derived from: the dialog state, the
/// per-fork CSeq map, the fallback address and the INVITE's CSeq number with
/// its alignment padding. No room for a request (one is `SipRequest`-sized)
/// nor for a datagram buffer.
fn fork_seed_bound() -> usize {
    size_of::<StackDialog>()
        + size_of::<HashMap<String, u32>>()
        + size_of::<SocketAddr>()
        + size_of::<u64>()
}

#[test]
fn a_confirmed_caller_keeps_no_copy_of_its_invite_request_or_datagram() {
    let table = DialogTable::default();
    let kept = boxed_size(&table.won_invite);
    let bound = fork_seed_bound();
    assert!(
        kept <= bound,
        "a confirmed caller keeps {kept} B of its establishing INVITE, bound {bound} B \
         (a SipRequest alone is {} B)",
        size_of::<SipRequest>()
    );
}
