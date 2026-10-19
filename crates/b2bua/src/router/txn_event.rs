//! A transaction-layer event as the call sees it ([`call_event`]).

use b2bua_sdk::event::CallEvent;
use sip_txn::TransactionEvent;

/// `event`, the transaction layer's, as a [`CallEvent`]: a timeout names the
/// incarnation its Via marked.
pub(crate) fn call_event(event: TransactionEvent) -> CallEvent {
    match event {
        TransactionEvent::Message { message, src, matched_client_txn } => {
            CallEvent::Sip { message, src, matched_client_txn }
        }
        TransactionEvent::Cancelled {
            call_id,
            from_tag,
            invite_cseq,
            in_dialog,
            headers,
            to_tag,
        } => CallEvent::Cancelled { call_id, from_tag, invite_cseq, in_dialog, headers, to_tag },
        TransactionEvent::Timeout {
            branch,
            call_ref,
            leg_id,
            incarnation_mark,
            method,
            destination,
            kind,
        } => {
            let incarnation = call_ref
                .as_deref()
                .zip(incarnation_mark.as_deref())
                .map(|(call_ref, mark)| call::derive_incarnation(call_ref, mark));
            CallEvent::Timeout {
                branch,
                call_ref,
                leg_id,
                method,
                destination,
                timeout_kind: kind,
                incarnation,
            }
        }
        TransactionEvent::CallQuiesced { call_ref } => CallEvent::CallQuiesced { call_ref },
    }
}

#[cfg(test)]
mod tests {
    use call::TxnTimeoutKind;

    use super::*;

    fn timeout(call_ref: Option<&str>, mark: Option<&str>) -> CallEvent {
        call_event(TransactionEvent::Timeout {
            branch: "z9hG4bK-t".into(),
            call_ref: call_ref.map(str::to_string),
            leg_id: Some("b-1".into()),
            incarnation_mark: mark.map(str::to_string),
            method: Some("INVITE".into()),
            destination: None,
            kind: TxnTimeoutKind::Response,
        })
    }

    #[test]
    fn a_timeout_names_the_incarnation_its_via_marked() {
        assert_eq!(timeout(Some("w0|cid|tag"), Some("k3")).incarnation(), Some("w0|cid|tag#k3"));
        assert_eq!(timeout(Some("w0|cid|tag"), None).incarnation(), None, "unmarked");
        assert_eq!(timeout(None, Some("k3")).incarnation(), None, "attributed to no call");
    }
}
