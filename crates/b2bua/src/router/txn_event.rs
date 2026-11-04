//! A transaction-layer event as the call sees it ([`call_event`]).

use b2bua_sdk::event::{CallEvent, HOP_ALIVE_TOPIC};
use sip_txn::TransactionEvent;

/// `event`, the transaction layer's, as a [`CallEvent`], with whether a
/// response it carries answered a client transaction the layer holds: a
/// timeout names the incarnation its Via marked, and the notice that a
/// watched call's last transaction cleared is the self-release notice
/// ([`super::quiesced::notice`]); a hop reported alive is the
/// [`HOP_ALIVE_TOPIC`] internal event of its call. A CANCEL the layer answered is stamped
/// `now_ms`, the instant its event reached the run loop.
pub(crate) fn call_event(event: TransactionEvent, now_ms: i64) -> (CallEvent, bool) {
    let event = match event {
        TransactionEvent::Message { message, src, matched_client_txn, datagram } => {
            return (CallEvent::Sip { message, src, datagram }, matched_client_txn);
        }
        TransactionEvent::Cancelled {
            call_id,
            from_tag,
            invite_cseq,
            in_dialog,
            headers,
            to_tag,
        } => CallEvent::Cancelled {
            call_id,
            from_tag,
            invite_cseq,
            in_dialog,
            headers,
            to_tag,
            at_ms: now_ms,
        },
        TransactionEvent::Timeout {
            branch,
            call_ref,
            leg_id,
            incarnation_mark,
            method,
            cseq,
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
                cseq,
                destination,
                timeout_kind: kind,
                incarnation,
            }
        }
        TransactionEvent::HopAlive { branch, call_ref, leg_id, incarnation_mark } => {
            let incarnation =
                incarnation_mark.map(|mark| call::derive_incarnation(&call_ref, &mark));
            CallEvent::InternalEvent {
                call_ref,
                topic: HOP_ALIVE_TOPIC.to_string(),
                outcome: String::new(),
                payload: serde_json::json!({ "leg_id": leg_id, "branch": branch }),
                body: Vec::new(),
                incarnation,
            }
        }
        TransactionEvent::CallQuiesced { call_ref } => super::quiesced::notice(call_ref),
    };
    (event, false)
}

#[cfg(test)]
mod tests {
    use call::TxnTimeoutKind;

    use super::*;

    fn timeout(call_ref: Option<&str>, mark: Option<&str>) -> CallEvent {
        let (event, _) = call_event(
            TransactionEvent::Timeout {
                branch: "z9hG4bK-t".into(),
                call_ref: call_ref.map(str::to_string),
                leg_id: Some("b-1".into()),
                incarnation_mark: mark.map(str::to_string),
                method: Some("INVITE".into()),
                cseq: 1,
                destination: None,
                kind: TxnTimeoutKind::Response,
            },
            0,
        );
        event
    }

    #[test]
    fn a_timeout_names_the_incarnation_its_via_marked() {
        assert_eq!(timeout(Some("w0|cid|tag"), Some("k3")).incarnation(), Some("w0|cid|tag#k3"));
        assert_eq!(timeout(Some("w0|cid|tag"), None).incarnation(), None, "unmarked");
        assert_eq!(timeout(None, Some("k3")).incarnation(), None, "attributed to no call");
    }

    #[test]
    fn a_hop_reported_alive_is_its_calls_internal_event() {
        let (event, matched) = call_event(
            TransactionEvent::HopAlive {
                branch: "z9hG4bK-a".into(),
                call_ref: "w0|cid|tag".into(),
                leg_id: "b-1".into(),
                incarnation_mark: Some("k3".into()),
            },
            0,
        );
        assert!(!matched);
        let CallEvent::InternalEvent { call_ref, topic, payload, .. } = &event else {
            panic!("an internal event: {event:?}");
        };
        assert_eq!(call_ref, "w0|cid|tag");
        assert_eq!(topic, HOP_ALIVE_TOPIC);
        assert_eq!(payload["leg_id"], "b-1");
        assert_eq!(payload["branch"], "z9hG4bK-a");
        assert_eq!(event.incarnation(), Some("w0|cid|tag#k3"));
    }
}
