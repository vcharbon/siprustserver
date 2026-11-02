use super::*;
use sip_message::header::HeaderValue;
use sip_message::sip_str::SipStr;

fn entry(branch: &str) -> CancelEntry {
    CancelEntry {
        target: ProxyAddr::new("10.0.0.2", 5070),
        branch: branch.to_string(),
        stickiness: None,
        minted: None,
        via_cookie: None,
    }
}

fn via(line: &str) -> Via {
    Via::parse(&SipStr::owned(line)).expect("a Via value")
}

fn key(call_id: &str) -> String {
    invite_txn_key(&via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1"), call_id, Some("t"), 1)
}

/// A panic under the table's lock leaves the table serving: the next
/// caller takes it as it stands, and the recovery is counted.
#[test]
fn a_panic_under_the_table_lock_leaves_it_serving() {
    let bounds = MemoBounds { stripes: 1, ..MemoBounds::default() };
    let lru = Arc::new(CancelBranchLru::with_bounds(Clock::test_at(0), bounds));
    lru.remember(&key("before"), entry("z9hG4bK-before"), Lifetime::InviteTransaction);
    let poisoner = lru.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.stripes[0].lock();
        panic!("a panic under the table lock");
    })
    .join();
    let counted = || crate::poison::RECOVERIES.count(INVITE_BRANCH_LRU);
    let recovered = counted();
    assert_eq!(lru.size(), 1, "the table as it stood");
    lru.remember(&key("after"), entry("z9hG4bK-after"), Lifetime::InviteTransaction);
    assert!(lru.lookup(&key("after")).is_some());
    assert_eq!(counted() - recovered, 1, "the recovery is counted once, under its lock");
}

#[test]
fn key_format_is_branch_sent_by_then_dialog_fields() {
    let v = via("SIP/2.0/UDP Host.Example:5060;branch=z9hG4bK1;received=10.0.0.9;rport=4");
    assert_eq!(
        invite_txn_key(&v, "abc@h", Some("t1"), 7),
        "inv|z9hG4bK1|host.example:5060|abc@h|t1|7"
    );
    assert_eq!(
        invite_txn_key(&via("SIP/2.0/UDP h;branch=z9hG4bK1"), "abc@h", None, 7),
        "inv|z9hG4bK1|h|abc@h||7"
    );
    assert_eq!(
        ack_hop_key(&v, "abc@h", Some("t1"), 7),
        "ackhop|z9hG4bK1|host.example:5060|abc@h|t1|7"
    );
}

#[test]
fn the_received_transaction_disambiguates_one_dialog_identity() {
    // A spiral crosses the proxy twice under one Call-ID, From tag and
    // CSeq: the branch and the sent-by keep the two passes apart.
    let first = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKa");
    for other in [
        "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKb",
        "SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bKa",
        "SIP/2.0/UDP 10.0.0.1:5061;branch=z9hG4bKa",
        "SIP/2.0/UDP 10.0.0.1;branch=z9hG4bKa",
    ] {
        assert_ne!(
            invite_txn_key(&first, "c1", Some("t"), 5),
            invite_txn_key(&via(other), "c1", Some("t"), 5),
            "{other}"
        );
    }
}

#[test]
fn from_tag_disambiguates_the_two_dialog_directions() {
    // Both directions share the Call-ID; CSeq spaces are independent and
    // can collide on the same number — the From-tag keeps them apart.
    let v = via("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKa");
    assert_ne!(
        invite_txn_key(&v, "c1", Some("uac"), 5),
        invite_txn_key(&v, "c1", Some("b2bua"), 5)
    );
}

#[test]
fn remember_then_lookup_returns_entry() {
    let lru = CancelBranchLru::with_clock(Clock::test_at(0));
    let k = key("call-1");
    lru.remember(&k, entry("z9hG4bK-1"), Lifetime::Retransmission);
    assert_eq!(lru.lookup(&k).unwrap().branch, "z9hG4bK-1");
    assert_eq!(lru.size(), 1);
    assert!(lru.lookup("absent").is_none());
}

#[tokio::test(start_paused = true)]
async fn entries_expire_after_their_own_ttl() {
    let lru = CancelBranchLru::with_clock(Clock::test_at(0));
    let short = key("call-1");
    let long = key("call-2");
    lru.remember(&short, entry("a"), Lifetime::Retransmission);
    lru.remember(&long, entry("b"), Lifetime::InviteTransaction);
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS + 1)).await;
    assert!(lru.lookup(&short).is_none(), "short-TTL entry should have expired");
    assert!(lru.lookup(&long).is_some(), "long-TTL entry must outlive the short one");
    assert_eq!(lru.sweep_expired().removed, 0, "lazy lookup already evicted the expired one");
}

fn one_stripe(max_entries: usize, max_bytes: usize) -> CancelBranchLru {
    CancelBranchLru::with_bounds(
        Clock::test_at(0),
        MemoBounds { max_entries, max_bytes, stripes: 1 },
    )
}

async fn tick() {
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
}

fn retransmission_share(bounds: MemoBounds) -> usize {
    bounds.per_pool(Lifetime::Retransmission).0
}

/// Past its pool's share of the entry cap the memo forgets that pool's
/// oldest writes first and counts each one; the newest stay readable.
#[tokio::test(start_paused = true)]
async fn a_flood_past_the_entry_cap_evicts_the_oldest() {
    let lru = one_stripe(16, usize::MAX);
    let cap =
        retransmission_share(MemoBounds { max_entries: 16, max_bytes: usize::MAX, stripes: 1 });
    assert!(cap > 1 && cap < 16, "the pool holds a share of the cap: {cap}");
    for i in 0..20 {
        lru.remember(&key(&format!("flood-{i}")), entry("b"), Lifetime::Retransmission);
        tick().await;
    }
    assert_eq!(lru.size(), cap, "the pool stays at its share");
    assert_eq!(lru.evicted(), (20 - cap) as u64, "every forgotten entry is counted");
    for i in 0..20 - cap {
        assert!(lru.lookup(&key(&format!("flood-{i}"))).is_none(), "flood-{i} was evicted");
    }
    for i in 20 - cap..20 {
        assert!(lru.lookup(&key(&format!("flood-{i}"))).is_some(), "flood-{i} is kept");
    }
}

/// Keys as long as a header allows fill the byte budget before the entry
/// cap: the memo stays inside the budget, the oldest going first.
#[tokio::test(start_paused = true)]
async fn oversized_keys_stay_inside_the_byte_budget() {
    let budget = 64 * 1024;
    let lru = one_stripe(1_000, budget);
    let long = "x".repeat(4_000);
    for i in 0..100 {
        lru.remember(&key(&format!("{long}-{i}")), entry("b"), Lifetime::Retransmission);
        tick().await;
    }
    assert!(lru.bytes() <= budget, "{} bytes held past a {budget} budget", lru.bytes());
    assert!(lru.size() < 16, "{} oversized entries held", lru.size());
    assert_eq!(lru.evicted(), 100 - lru.size() as u64);
    assert!(lru.lookup(&key(&format!("{long}-99"))).is_some(), "the newest is kept");
    assert!(lru.lookup(&key(&format!("{long}-0"))).is_none(), "the oldest is evicted");
}

/// A flood of short-lived memos never pushes out an INVITE transaction's
/// entry: each lifetime holds its own share of the bounds.
#[tokio::test(start_paused = true)]
async fn a_flood_of_short_memos_keeps_a_ringing_invites_entry() {
    let lru = one_stripe(8, usize::MAX);
    lru.remember(&key("ringing"), entry("inv"), Lifetime::InviteTransaction);
    for i in 0..50 {
        tick().await;
        lru.remember(&key(&format!("flood-{i}")), entry("b"), Lifetime::Retransmission);
    }
    assert!(lru.evicted() > 0, "the flood evicts its own memos");
    assert_eq!(lru.lookup(&key("ringing")).map(|e| e.branch), Some("inv".to_string()));
}

/// A rewritten key holds one entry, its expiry taken from the last write.
#[tokio::test(start_paused = true)]
async fn a_rewrite_refreshes_one_entry() {
    let lru = one_stripe(8, usize::MAX);
    lru.remember(&key("c"), entry("first"), Lifetime::Retransmission);
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS - 1)).await;
    lru.remember(&key("c"), entry("second"), Lifetime::Retransmission);
    assert_eq!(lru.size(), 1);
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS - 1)).await;
    assert_eq!(lru.sweep_expired().removed, 0, "the rewrite moved the expiry");
    assert_eq!(lru.lookup(&key("c")).map(|e| e.branch), Some("second".to_string()));
    tick().await;
    assert!(lru.lookup(&key("c")).is_none());
    assert_eq!(lru.size(), 0);
    assert_eq!(lru.bytes(), 0, "nothing is accounted once the table is empty");
}

/// An expiry pass looks at the entries that expired and at the one
/// entry per stripe that ends it, whatever the size of the table.
#[tokio::test(start_paused = true)]
async fn an_expiry_pass_inspects_what_expired_not_the_table() {
    let lru = one_stripe(usize::MAX, usize::MAX);
    for i in 0..1_000 {
        lru.remember(&key(&format!("long-{i}")), entry("b"), Lifetime::InviteTransaction);
    }
    for i in 0..10 {
        lru.remember(&key(&format!("short-{i}")), entry("b"), Lifetime::Retransmission);
    }
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS + 1)).await;
    assert_eq!(lru.sweep_expired(), Swept { removed: 10, inspected: 11 });
    assert_eq!(lru.size(), 1_000);
}

#[tokio::test(start_paused = true)]
async fn sweep_drops_expired() {
    let lru = CancelBranchLru::with_clock(Clock::test_at(0));
    lru.remember(&key("c1"), entry("a"), Lifetime::Retransmission);
    lru.remember(&key("c2"), entry("b"), Lifetime::Retransmission);
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS + 1)).await;
    assert_eq!(lru.sweep_expired().removed, 2);
    assert_eq!(lru.size(), 0);
}

/// A cookie as the load balancer mints it (`strategies::load_balancer`).
fn cookie() -> RouteParams {
    [
        ("w_pri", "b2bua-worker-7f9c8d6b5d-x2k4p"),
        ("w_bak", "b2bua-worker-7f9c8d6b5d-q8z1m"),
        ("e", "0"),
        ("v", "1"),
        ("kid", "k0"),
        ("sig", "Qm9ndXMtc2lnbmF0dXJlLTEy"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
}

/// The defaults: 3/8 of a 512 MiB pod in accounted bytes, as many entries
/// of the heaviest shape the request path writes (a worker's INVITE
/// retransmission memo with its minted and its Via cookie) as those bytes
/// hold, and the call rate that working set stands for.
#[test]
fn the_default_bounds_hold_the_heaviest_entries_in_their_bytes() {
    let d = MemoBounds::default();
    assert_eq!(d.max_bytes, 192 << 20);
    assert_eq!(d.max_entries, d.max_bytes / TYPICAL_ENTRY_BYTES);
    assert_eq!(d.stripes, DEFAULT_STRIPES);
    let via = via("SIP/2.0/UDP 10.244.5.8:5060;branch=z9hG4bK-b2bua-4f3c1b2a9e8d7c6b5a;lg=b;rport");
    let heaviest = CancelEntry {
        target: ProxyAddr::new("203.0.113.40", 5060),
        branch: "z9hG4bK.4f3c1b2a9e8d7c6b5a4f3e2d1c0b".to_string(),
        stickiness: None,
        minted: Some(cookie()),
        via_cookie: Some(cookie()),
    };
    let rtx = retransmit_key(
        &via,
        "b2bua-c4f1e2a3b4c5d6e7f8a9@10.244.5.8",
        Some("b2b-7f3e9a"),
        "INVITE",
        1,
    );
    let bytes = super::footprint::footprint(&rtx, &heaviest);
    assert!(bytes <= TYPICAL_ENTRY_BYTES, "the heaviest entry accounts {bytes} bytes");
    assert!(bytes > TYPICAL_ENTRY_BYTES * 9 / 10, "the typical figure is that entry's: {bytes}");
    assert_eq!(calls_per_second(&d), 109);
}

/// A flood of INVITE entries never pushes out a short memo: each lifetime
/// class holds its own share of the bounds.
#[tokio::test(start_paused = true)]
async fn a_flood_of_invite_entries_keeps_a_short_memo() {
    let lru = one_stripe(8, usize::MAX);
    lru.remember(&key("rtx"), entry("short"), Lifetime::Retransmission);
    for i in 0..50 {
        tick().await;
        lru.remember(&key(&format!("invite-{i}")), entry("b"), Lifetime::InviteTransaction);
    }
    assert_eq!(lru.lookup(&key("rtx")).map(|e| e.branch), Some("short".to_string()));
}
