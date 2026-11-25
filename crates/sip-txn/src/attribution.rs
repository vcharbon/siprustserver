//! Whether a request can belong to a call by what it states: the `callRef`
//! of its Request-URI, which an element's Contact stamps and the in-dialog
//! requests of its calls carry back. Every stage that sheds the requests that
//! name no call first reads it the same way: the transaction layer's
//! unattributed lane ([`TransactionConfig::unattributed_lane`](crate::TransactionConfig))
//! and any stage ahead of the layer.

use sip_message::header::ParamValue;
use sip_message::{Method, SipRequest};

/// Whether `req` can belong to no call the element names: its Request-URI
/// states no `callRef` value, and it is neither an initial INVITE (no
/// To-tag: a new call, judged by admission) nor a CANCEL (one that matches no
/// INVITE may cancel one a takeover rebuilds). A request of a call whose
/// Request-URI an element on its path rewrote reads the same.
pub fn names_no_call(req: &SipRequest) -> bool {
    let initial_invite = req.method() == Method::Invite && req.to().tag().is_none();
    let cancel = req.method() == Method::Cancel;
    let names_a_call = req.request_uri().param("callRef").and_then(ParamValue::as_str).is_some();
    !(initial_invite || cancel || names_a_call)
}
