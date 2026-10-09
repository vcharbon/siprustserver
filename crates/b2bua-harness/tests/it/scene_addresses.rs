//! A `B2buaScene` takes its addresses from its harness's allocator: alice,
//! bob and the b2bua are its first three, the canonical ports, and a further
//! party takes the next free one.

use b2bua_harness::{B2buaScene, ALICE_PORT, B2BUA_PORT, BOB_PORT};

#[tokio::test(start_paused = true)]
async fn the_scene_holds_the_first_three_addresses_and_a_new_party_the_next() {
    let s = B2buaScene::new("scene-addresses").await;
    assert_eq!(
        [s.alice.addr().port(), s.bob.addr().port(), s.b2bua.addr.port()],
        [ALICE_PORT, BOB_PORT, B2BUA_PORT],
    );
    let carol = s.h.agent_auto("carol").await;
    assert_eq!(carol.addr(), "127.0.0.1:5090".parse().unwrap());
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    s.finish().await;
}
