//! Port of `tests/sip/transaction-layer-handles.test.ts` — `send_request`
//! returns a typed handle carrying the Via branch, original request, and
//! destination (consumed by later CANCEL / ACK-for-2xx call sites).

mod common;
use common::*;
use sip_txn::{ClientTransactionHandle, TxnKind};

#[tokio::test(start_paused = true)]
async fn send_request_invite_returns_invite_handle() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bKhandle-invite";
    let invite = outbound_request("INVITE", branch);

    let handle = stack
        .txn
        .send_request(invite.clone(), addr("192.0.2.20:5060"), TxnKind::Invite)
        .await;

    match handle {
        ClientTransactionHandle::Invite {
            branch: b,
            original_invite,
            destination,
        } => {
            assert_eq!(b, branch);
            assert_eq!(destination, addr("192.0.2.20:5060"));
            assert_eq!(original_invite, invite);
        }
        _ => panic!("expected an Invite handle"),
    }
    assert_eq!(stack.txn.metrics().active_transactions(), 1);
}

#[tokio::test(start_paused = true)]
async fn send_request_non_invite_returns_non_invite_handle() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bKhandle-bye";
    let bye = outbound_request("BYE", branch);

    let handle = stack
        .txn
        .send_request(bye.clone(), addr("192.0.2.20:5060"), TxnKind::NonInvite)
        .await;

    match handle {
        ClientTransactionHandle::NonInvite {
            branch: b,
            original_request,
            destination,
        } => {
            assert_eq!(b, branch);
            assert_eq!(destination, addr("192.0.2.20:5060"));
            assert_eq!(original_request, bye);
        }
        _ => panic!("expected a NonInvite handle"),
    }
}
