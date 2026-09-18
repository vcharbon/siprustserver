//! Pure, correct-by-default SIP message constructors. Every returned
//! [`SipRequest`](crate::types::SipRequest) /
//! [`SipResponse`](crate::types::SipResponse) is immediately sendable: no
//! sentinels, no post-processing; Call-ID, branch, tag, local address and
//! CSeq are arguments, never side effects. Reading *existing* messages does
//! NOT live here — that is the typed read surface on the message itself;
//! lenient raw scanning is [`crate::sniff`].
//!
//! Each generator is a recipe over a [`Draft`](crate::draft::Draft): it knows
//! the dialog rules — CSeq stepping, route-set application, tag placement — and
//! leaves the wire bytes to one render at freeze, so a built message carries a
//! real image and is read back exactly like a parsed one. Every header input is
//! a typed value ([`Via`](crate::header::Via),
//! [`From`](crate::header::From), …) or — where the RFC makes the header an
//! echo of bytes the caller already holds — a draft
//! [`Entry`](crate::draft::Entry); no `Generate*Opts` field names a header as
//! text, so nothing reaches the wire through a re-parse.
//!
//! Concern map:
//!   - [`spec`] — non-header input shapes (dialog / INVITE-txn views)
//!   - [`methods`] — admissible-method views
//!   - [`capabilities`] — the advertised Allow/Supported/Accept set
//!   - [`contact_policy`] — where the stack states its own Contact
//!   - [`out_of_dialog`] — initial INVITE / one-shot requests (§8.1.1)
//!   - [`in_dialog`] — in-dialog requests + route-set computation (§12.2.1.1)
//!   - [`ack`] — ACK for 2xx (§13.2.2.4) and for non-2xx finals (§17.1.1.3)
//!   - [`cancel`] — CANCEL of an outstanding INVITE (§9.1)
//!   - [`response`] — UAS responses echoing the request (§8.2.6.2)
//!   - [`relay`] — B2BUA relay transparency + rebuilt responses (§16.6)

mod emit;

pub mod ack;
pub mod cancel;
pub mod capabilities;
pub mod contact_policy;
pub mod in_dialog;
pub mod methods;
pub mod out_of_dialog;
pub mod relay;
pub mod response;
pub mod spec;

pub use ack::{
    generate_ack_for_2xx, generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx,
    GenerateAckFor2xxOpts,
};
pub use cancel::generate_cancel;
pub use capabilities::{CapabilitySet, B2BUA_ACCEPT, B2BUA_ALLOW, B2BUA_SUPPORTED};
pub use contact_policy::{request_states_contact, response_states_contact};
pub use in_dialog::{generate_in_dialog_request, GenerateInDialogRequestOpts, InDialogResult};
pub use methods::{InDialogMethod, OutOfDialogMethod};
pub use out_of_dialog::{generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts};
pub use relay::{
    generate_relayed_response, relayable, relayable_headers, states_send_time,
    GenerateRelayedResponseOpts, RelayScope, RelayTarget, SourceBody,
};
pub use response::{generate_response, GenerateResponseOpts};
pub use spec::{InviteClientTransactionHandle, StackDialog};
