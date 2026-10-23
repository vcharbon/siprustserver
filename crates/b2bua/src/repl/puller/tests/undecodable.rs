//! A replicated body the codec refuses is counted, never dropped in silence:
//! the call it carries has no usable replica on this node.

use std::sync::Arc;

use call::CallBodyCodec;

use super::{backup_puller, body_at, forward};

/// `body_at` an answered call whose service JSON a peer nested `depth`
/// levels deeper.
fn nested_body(depth: usize) -> Arc<[u8]> {
    let codec = call::MsgpackCodec::new();
    let call = codec.decode(&body_at(Some(200), call::CallModelState::Active)).expect("decodes");
    let call = call::helpers::set_call_ext(call, "svc", Some(1.into()));
    let mut body = codec.encode(&call).expect("encodes");
    let key = b"\xa3svc";
    let at = body.windows(key.len()).position(|w| w == key).expect("the service key") + key.len();
    body.splice(at..at, std::iter::repeat_n(0x91, depth));
    Arc::from(body)
}

#[tokio::test]
async fn a_forward_body_nested_past_the_bound_is_counted() {
    let (p, _, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    forward(&p, (1, 0), nested_body(4)).await;
    assert_eq!(p.metrics.repl_body_undecodable_total(), 0, "a body within the bound decodes");
    forward(&p, (2, 0), nested_body(call::codec::MAX_DEPTH)).await;
    assert_eq!(p.metrics.repl_body_undecodable_total(), 1, "the refused body is counted");
}
