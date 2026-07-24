//! Pure, correct-by-default SIP message constructors. Every returned
//! [`SipRequest`](crate::types::SipRequest) /
//! [`SipResponse`](crate::types::SipResponse) is immediately sendable: no
//! sentinels, no post-processing; Call-ID, branch, tag, local address and
//! CSeq are arguments, never side effects. Reading/rewriting *existing*
//! messages does NOT live here — see [`crate::message_helpers`]; lenient raw
//! scanning is [`crate::sniff`].
//!
//! Concern map:
//!   - [`spec`] — input shapes (transport, Via/Contact specs, dialog/txn views)
//!   - [`methods`] — admissible-method views + advertised Allow/Supported
//!   - [`out_of_dialog`] — initial INVITE / one-shot requests (§8.1.1)
//!   - [`in_dialog`] — in-dialog requests + route-set computation (§12.2.1.1)
//!   - [`ack`] — ACK for 2xx (§13.2.2.4) and for non-2xx finals (§17.1.1.3)
//!   - [`cancel`] — CANCEL of an outstanding INVITE (§9.1)
//!   - [`response`] — UAS responses echoing the request (§8.2.6.2)
//!   - [`relay`] — B2BUA relay transparency + rebuilt responses (§16.6)

mod emit;

pub mod ack;
pub mod cancel;
pub mod in_dialog;
pub mod methods;
pub mod out_of_dialog;
pub mod relay;
pub mod response;
pub mod spec;

pub use ack::{generate_ack_for_2xx, generate_ack_for_non_2xx, GenerateAckFor2xxOpts};
pub use cancel::generate_cancel;
pub use in_dialog::{generate_in_dialog_request, GenerateInDialogRequestOpts, InDialogResult};
pub use methods::{InDialogMethod, OutOfDialogMethod, B2BUA_ALLOW, B2BUA_SUPPORTED};
pub use out_of_dialog::{generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts};
pub use relay::{
    extract_non_structural_headers, generate_relayed_response, GenerateRelayedResponseOpts,
};
pub use response::{generate_response, GenerateResponseOpts};
pub use spec::{ContactSpec, InviteClientTransactionHandle, SipTransport, StackDialog, ViaSpec};

// Route/Via value readers and rewriters live on the read/rewrite side
// (message_helpers); these re-exports keep the long-standing generator paths.
pub use crate::message_helpers::route::{first_route_is_loose, strip_route_uri_to_request_uri};
pub use crate::message_helpers::via::stamp_received_rport_on_via;
