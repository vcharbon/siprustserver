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
    let lru = Arc::new(CancelBranchLru::with_clock(Clock::test_at(0)));
    lru.remember(&key("before"), entry("z9hG4bK-before"), 60_000);
    let poisoner = lru.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.table.lock();
        panic!("a panic under the table lock");
    })
    .join();
    let counted = || crate::poison::RECOVERIES.count(INVITE_BRANCH_LRU);
    let recovered = counted();
    assert_eq!(lru.size(), 1, "the table as it stood");
    lru.remember(&key("after"), entry("z9hG4bK-after"), 60_000);
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
    lru.remember(&k, entry("z9hG4bK-1"), 1000);
    assert_eq!(lru.lookup(&k).unwrap().branch, "z9hG4bK-1");
    assert_eq!(lru.size(), 1);
    assert!(lru.lookup("absent").is_none());
}

#[tokio::test(start_paused = true)]
async fn entries_expire_after_their_own_ttl() {
    let lru = CancelBranchLru::with_clock(Clock::test_at(0));
    let short = key("call-1");
    let long = key("call-2");
    lru.remember(&short, entry("a"), 1000);
    lru.remember(&long, entry("b"), 5000);
    tokio::time::advance(std::time::Duration::from_millis(1001)).await;
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

/// Past its entry cap the memo forgets the oldest entries first and
/// counts each one it forgot; the newest stay readable.
#[tokio::test(start_paused = true)]
async fn a_flood_past_the_entry_cap_evicts_the_oldest() {
    let lru = one_stripe(8, usize::MAX);
    for i in 0..20 {
        lru.remember(&key(&format!("flood-{i}")), entry("b"), RTX_ENTRY_TTL_MS);
        tick().await;
    }
    assert_eq!(lru.size(), 8, "the table stays at its cap");
    assert_eq!(lru.evicted(), 12, "every forgotten entry is counted");
    for i in 0..12 {
        assert!(lru.lookup(&key(&format!("flood-{i}"))).is_none(), "flood-{i} was evicted");
    }
    for i in 12..20 {
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
        lru.remember(&key(&format!("{long}-{i}")), entry("b"), RTX_ENTRY_TTL_MS);
        tick().await;
    }
    assert!(lru.bytes() <= budget, "{} bytes held past a {budget} budget", lru.bytes());
    assert!(lru.size() < 16, "{} oversized entries held", lru.size());
    assert_eq!(lru.evicted(), 100 - lru.size() as u64);
    assert!(lru.lookup(&key(&format!("{long}-99"))).is_some(), "the newest is kept");
    assert!(lru.lookup(&key(&format!("{long}-0"))).is_none(), "the oldest is evicted");
}

/// A flood of short-lived memos never pushes out an INVITE transaction's
/// entry with time left: the entry nearest its expiry goes first.
#[tokio::test(start_paused = true)]
async fn a_flood_of_short_memos_keeps_a_ringing_invites_entry() {
    let lru = one_stripe(8, usize::MAX);
    lru.remember(&key("ringing"), entry("inv"), INVITE_ENTRY_TTL_MS);
    for i in 0..50 {
        tick().await;
        lru.remember(&key(&format!("flood-{i}")), entry("b"), RTX_ENTRY_TTL_MS);
    }
    assert_eq!(lru.size(), 8);
    assert_eq!(lru.lookup(&key("ringing")).map(|e| e.branch), Some("inv".to_string()));
}

/// A rewritten key holds one entry, its expiry taken from the last write.
#[tokio::test(start_paused = true)]
async fn a_rewrite_refreshes_one_entry() {
    let lru = one_stripe(8, usize::MAX);
    lru.remember(&key("c"), entry("first"), RTX_ENTRY_TTL_MS);
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS - 1)).await;
    lru.remember(&key("c"), entry("second"), RTX_ENTRY_TTL_MS);
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
        lru.remember(&key(&format!("long-{i}")), entry("b"), INVITE_ENTRY_TTL_MS);
    }
    for i in 0..10 {
        lru.remember(&key(&format!("short-{i}")), entry("b"), RTX_ENTRY_TTL_MS);
    }
    tokio::time::advance(std::time::Duration::from_millis(RTX_ENTRY_TTL_MS + 1)).await;
    assert_eq!(lru.sweep_expired(), Swept { removed: 10, inspected: 11 });
    assert_eq!(lru.size(), 1_000);
}

#[tokio::test(start_paused = true)]
async fn sweep_drops_expired() {
    let lru = CancelBranchLru::with_clock(Clock::test_at(0));
    lru.remember(&key("c1"), entry("a"), 1000);
    lru.remember(&key("c2"), entry("b"), 1000);
    tokio::time::advance(std::time::Duration::from_millis(1001)).await;
    assert_eq!(lru.sweep_expired().removed, 2);
    assert_eq!(lru.size(), 0);
}
