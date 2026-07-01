//! S11 (ADR-0011 X11 / ADR-0014) fail-back tests — the `CallState`-level
//! mechanics the active-reclaim + acting-backup **self-release** is built on:
//!
//! 1. **takeover tagging** — `mark_takeover` flags an acting-backup copy;
//!    `is_takeover` is the flag the router reads to drive self-release once the
//!    served transaction(s) clear (the SIP-level wiring is the failover harness);
//! 2. **local-only self-release** — `drop_local` sheds the live copy WITHOUT
//!    propagating a delete: the backup `Element` survives (the call lives on at
//!    its reclaiming primary) and the takeover flag clears;
//! 3. **active reclaim read-paths** — `reclaim_scan` (bulk) + `peek_reclaimable`
//!    (reactive straggler) decode this node's `pri:` partition, and
//!    `materialize_if_absent` inserts idempotently.
//!
//! These exercise the seams directly (no full SIP failover harness — that is the
//! `failover-harness` acceptance). The `Deactivate` watermark handshake these
//! tests once covered was removed with eager takeover (ADR-0014).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallBodyCodec, MsgpackCodec};
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::ReplicatingCallStore;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::store::{
    BufferedTerminateWriter, CallState, CallStore, InMemoryCallStore, PartitionRole, PutOpts,
};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn config_for(ordinal: &str) -> B2buaConfig {
    B2buaConfig {
        self_ordinal: ordinal.into(),
        ..Default::default()
    }
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A proxied INVITE carrying the `w_pri`/`w_bak` stickiness cookie, parametrised
/// by Call-ID so each call gets a distinct `callRef` (`{pri}|{cid}|alicetag`).
fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// The end-of-hold in-dialog BYE the UAC sends after its keepalive hold — the
/// request that resolves to the same `callRef` as [`invite`] (same Call-ID +
/// alice tag) and, on the rebooted primary, gets the 481. `CSeq: 2 BYE` mirrors
/// the endurance `-trace_err` log (the BYE is the 2nd request of the dialog).
fn bye(cid: &str) -> SipRequest {
    let raw = format!(
        "BYE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-bye-{cid}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>;tag=bobtag\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A `CallState` for `ordinal` wired to `repl` as its replicating store (mirrors
/// how `B2buaCore` builds it: an in-memory `store` + the replicating one).
fn call_state(ordinal: &str, repl: Arc<ReplicatingCallStore>) -> CallState {
    let store = Arc::new(InMemoryCallStore::new()) as Arc<dyn CallStore>;
    let writer = BufferedTerminateWriter::spawn(store.clone(), 1024);
    CallState::new(store, writer, ordinal, B2buaMetrics::new()).with_replication(repl)
}

/// Seed a call body into `(role, primary)` of the replicating store.
async fn put(store: &ReplicatingCallStore, role: PartitionRole, primary: &str, call: &Call) {
    let body = MsgpackCodec::new().encode(call);
    let gen = call.topology.as_ref().map(|t| t.gen).unwrap_or(1);
    store
        .put_call(role, primary, &call.call_ref, body, &[], 60_000, gen, 0, &PutOpts::default())
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// (1) takeover tagging: mark_takeover sets the flag the router self-releases on.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn mark_takeover_flags_the_copy_for_self_release() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w1", repl.clone());

    let c0 = build_initial_call(&invite("w0", "w1", "cid-a"), src(), &config_for("w0"), 0);
    let r0 = c0.call_ref.clone();
    assert!(r0.starts_with("w0|"));
    state.create(c0);

    assert!(!state.is_takeover(&r0), "a freshly-created call is not a takeover copy");
    state.mark_takeover(&r0);
    assert!(state.is_takeover(&r0), "mark_takeover flags it for self-release");
    // A ref we never marked is not a takeover.
    assert!(!state.is_takeover("w0|other|t"));
}

// ---------------------------------------------------------------------------
// (2) local-only self-release: shed the live copy, keep the backup Element.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn drop_local_sheds_live_copy_but_keeps_backup_element() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w1", repl.clone());

    // A call w0 owns + we (w1) back up: seed bak:w0, then hydrate the takeover.
    let call = build_initial_call(&invite("w0", "w1", "cid-h"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    put(&repl, BAK, "w0", &call).await;

    let (_c, fresh, _skew) = state.hydrate_from_replica(&r).await.expect("hydrate from bak:w0");
    assert!(fresh, "first hydrate materialises a fresh takeover copy");
    state.mark_takeover(&r);
    assert!(state.peek(&r).is_some(), "takeover copy is live");
    assert!(state.is_takeover(&r), "flagged as a takeover copy");

    // Self-release (local-only).
    assert!(state.drop_local(&r), "dropped a live copy");
    assert!(state.peek(&r).is_none(), "live copy gone from the map");
    assert!(!state.is_takeover(&r), "takeover flag cleared by drop_local");
    // The crux: NO delete propagated — the backup Element survives so the call
    // lives on at its reclaiming primary.
    assert!(
        repl.get_call(BAK, "w0", &r).await.unwrap().is_some(),
        "bak:w0 Element untouched by the local-only handback"
    );
    // Dropping a ref we no longer hold is a harmless no-op.
    assert!(!state.drop_local(&r), "second drop reports nothing dropped");
}

// ---------------------------------------------------------------------------
// (3) active reclaim read-paths + idempotent materialisation.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn reclaim_scan_materialises_pri_partition_idempotently() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w0", repl.clone());

    // A call w0 reclaimed into its own pri:w0 partition (via bootstrap).
    let call = build_initial_call(&invite("w0", "w1", "cid-r"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    put(&repl, PRI, "w0", &call).await;

    // Bulk sweep sees it.
    let scanned = state.reclaim_scan().await;
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].0.call_ref, r);

    // Materialise into the live map: first inserts, second is a no-op.
    assert!(state.materialize_if_absent(scanned[0].0.clone()), "first materialise inserts");
    assert!(state.peek(&r).is_some(), "now live + routable");
    assert!(!state.materialize_if_absent(scanned[0].0.clone()), "second materialise is a no-op");

    // Reactive read-path returns the same call; a backup-role ref never reclaims.
    assert_eq!(state.peek_reclaimable(&r).await.map(|(c, _)| c.call_ref), Some(r.clone()));
    assert!(
        state.peek_reclaimable("w5|other|t").await.is_none(),
        "a ref whose primary isn't us is not reclaimable here"
    );
}

// ===========================================================================
// REPRODUCTION — the endurance "long-hold dialogs die on B2BUA reboot" defect
// (study `deploy/k8s/results/endurance-20260605-165318/long-call-failure-study.html`).
//
// The two tests below recreate, at this exact seam, the decision that produces
// the `481 Call/Transaction Does Not Exist` on the end-of-hold BYE. They split
// the failure into its TWO underlying conditions so we can reason about the fix
// separately — because the recommended fix only addresses ONE of them.
//
// Causal chain (verified in the study, store/mod.rs:160 + router.rs:496,636):
//   reboot → reclaim incomplete → BYE routed back to the (Ready) primary →
//   hydrate_from_replica sees a PRIMARY-role miss → returns None (store/mod.rs:170)
//   → process() falls into maybe_reject_orphan (router.rs:527,636) → 481.
// ===========================================================================

/// CASE A — the body WAS pulled into `pri:{self}` but never materialised (the
/// flip-race straggler: the one-shot `reclaim_all` already swept before the
/// import landed, and only a backup reverse-flush `ReclaimCall` — never an
/// arriving in-dialog request — re-materialised a post-sweep straggler).
///
/// FIXED at the router: `process()` now wires `peek_reclaimable` into the
/// hydrate-miss path (ON-DEMAND reclaim — router.rs, "REBOOTED-PRIMARY
/// on-demand reclaim"), so the arriving BYE materialises + serves the call
/// instead of orphan-481ing. This test pins the STATE-LEVEL seam contract the
/// router fix builds on: `hydrate_from_replica` itself still (correctly)
/// refuses a primary-role miss — the backup partition is a takeover source,
/// `pri:{self}` is a reclaim source read by `peek_reclaimable` — and the body
/// is reachable through the latter. The end-to-end recovery is asserted by
/// `failover-harness::in_dialog_bye_races_bulk_reclaim_served_on_demand`.
#[tokio::test]
async fn reboot_primary_481s_bye_for_unmaterialised_pri_call() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    // The rebooted node is the call's OWN primary (w0).
    let state = call_state("w0", repl.clone());

    // A long-hold call w0 owns. After reboot its body is present in pri:w0 (the
    // bootstrap delivered it) but it was NOT materialised into the live map.
    let call = build_initial_call(&invite("w0", "w1", "cid-long"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    assert!(r.starts_with("w0|"), "call_ref encodes w0 as the primary");
    put(&repl, PRI, "w0", &call).await; // pri:w0 body present …
    assert!(state.peek(&r).is_none(), "… but the live map MISSES it (un-reclaimed)");

    // The end-of-hold BYE arrives. The router resolves it through exactly this
    // call — `process()` (router.rs:496) on an in-dialog request.
    let resolved = state.hydrate_from_replica(&r).await;
    assert!(
        resolved.is_none(),
        "REPRO: a primary-role miss returns None (store/mod.rs:170) → \
         maybe_reject_orphan → 481 on the BYE — the endurance long-call loss"
    );

    // Render the LITERAL response the UAC receives — exactly what
    // `maybe_reject_orphan` (router.rs:640) emits when the resolve above is None.
    // This is the message in the endurance `/uac-long-options_1_errors.log`.
    let the_481 = generate_response(
        &bye("cid-long"),
        481,
        "Call/Transaction Does Not Exist",
        &GenerateResponseOpts::default(),
    );
    eprintln!(
        "\n──── actual message the UAC receives for its end-of-hold BYE ────\n\
         SIP/2.0 {} {}\n  CSeq: {} {}\n  Call-ID: {}\n\
         ────────────────────────────────────────────────────────────────\n",
        the_481.status,
        the_481.reason,
        the_481.cseq.seq,
        the_481.cseq.method,
        get_header(&the_481.headers, "call-id").unwrap_or(""),
    );
    assert_eq!(the_481.status, 481, "the UAC gets 481, not the 200 it expects");

    // The body IS locally present and reclaimable: the resolve path simply
    // refuses to look. `peek_reclaimable` (today reachable ONLY via a backup's
    // reverse-flush ReclaimCall push, router.rs:121 — never from an arriving
    // request) WOULD recover it. This is what the recommended fix wires in.
    assert_eq!(
        state.peek_reclaimable(&r).await.map(|(c, _)| c.call_ref),
        Some(r.clone()),
        "the call sits fully reclaimable in pri:w0 on THIS node"
    );
}

/// CASE B — the body was NEVER pulled into `pri:{self}` (the bootstrap pull
/// itself truncated: the study's `repl_reclaimed_total = 392` of ~2350, the
/// dominant production case). The call's only surviving copy is `bak:w0` on the
/// PEER. The same 481 results — but here the recommended local-read fix would
/// ALSO return None, so it does NOT recover this population.
///
/// This is the crux for the fix discussion: a fix that only reads the local
/// `pri:{self}` partition is blind to a call the truncated bootstrap never
/// imported. Recovering it needs an on-demand PULL from the peer's `bak:{self}`
/// (the same source bootstrap uses), or a bootstrap that does not truncate.
#[tokio::test]
async fn reboot_primary_481s_bye_when_pri_body_was_never_pulled() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w0", repl.clone());

    // Build the ref the same way, but seed NOTHING into this node's pri:w0 (the
    // truncated bootstrap never imported it). It lives only in bak:w0 on w1.
    let call = build_initial_call(&invite("w0", "w1", "cid-long2"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    assert!(state.peek(&r).is_none(), "not live (never reclaimed)");

    let resolved = state.hydrate_from_replica(&r).await;
    assert!(resolved.is_none(), "REPRO: same 481 on the BYE");

    // CRUX: pri:w0 is EMPTY on this node, so the recommended fix (read local
    // pri:{self} on a primary-role miss) would ALSO return None → still 481.
    assert!(
        state.peek_reclaimable(&r).await.is_none(),
        "the recommended local-read fix is BLIND to this population: the body \
         was never imported into pri:w0 — it survives only as bak:w0 on the peer"
    );
}

// ===========================================================================
// #4 REPRODUCTION — the Backup-flow bootstrap silently OMITS a reclaimed call.
//
// The server side of a peer's `PullRequest[Backup] caller=w1 since=(0,0)` answers
// from `scan_refs_backed_by`, which filters `pri:{self}` on the DENORMALISED
// `CallMeta.backup`. That field is captured ONLY on a Forward flush
// (`direction == Forward ⇒ opts.peer` IS the backup); the reboot-reclaim
// hydration path imports the body through the *peerless* `PutOpts::default()`
// (`apply_to_store`, puller.rs), so a just-reclaimed call lands `backup == None`
// and is INVISIBLE to w1's bootstrap until the call's next keepalive
// forward-flush. In that window the reclaimed call runs UN-BACKED-UP — a second
// primary crash loses it. The reclaim materialisation must re-establish `backup`
// from the authoritative `topology.bak` (the same value the proxy `w_bak` cookie
// carries), so a peer re-pulling what it must back up receives the call at once.
// ===========================================================================
#[tokio::test]
async fn reclaimed_call_is_visible_to_backup_bootstrap() {
    use super::changelog::BodySource;

    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w0", repl.clone());

    // A call w0 owns, backed up by w1 (topology.bak = w1, from the w_bak cookie).
    let call = build_initial_call(&invite("w0", "w1", "cid-bak"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    assert_eq!(
        call.topology.as_ref().map(|t| t.bak.as_str()),
        Some("w1"),
        "the w_bak cookie names w1 as this call's backup"
    );

    // Reboot-reclaim hydration: the puller imports the pri:w0 body via the
    // peerless `PutOpts::default()` path, so `CallMeta.backup` lands None.
    put(&repl, PRI, "w0", &call).await;
    assert!(
        repl.scan_refs_backed_by("w0", "w1").is_empty(),
        "pre-materialise: the peerless hydration left backup=None → invisible"
    );

    // Materialise the reclaimed call into the live serving map (what `reclaim_all`
    // / the reactive `ReclaimCall` do on reboot) — this must re-establish the
    // denormalised backup from `topology.bak`.
    let scanned = state.reclaim_scan().await;
    assert_eq!(scanned.len(), 1);
    assert!(state.materialize_if_absent(scanned[0].0.clone()), "first materialise inserts");

    // The reclaimed call is now visible to w1's Backup-flow bootstrap — a peer
    // re-pulling what it must back up receives it immediately, not one keepalive late.
    assert_eq!(
        repl.scan_refs_backed_by("w0", "w1"),
        vec![r.clone()],
        "#4: a reclaimed pri:{{self}} call MUST be visible to its backup's bootstrap"
    );
}

// ---------------------------------------------------------------------------
// (5) CONCURRENCY (unit, no SIP): a bulk reclaim runs WHILE create/update/delete
// churn hammers the SAME `CallState` on a multi-thread runtime. This drives the
// state-level mutations the router makes — reclaim's `reclaim_scan → lock →
// materialize_if_absent` (the core of `reclaim_into_live`) racing live
// `create`/`update`/`remove`, each under the per-call `lock` the dispatcher
// serialises on in production — under TRUE parallelism. It is the unit-level
// answer to "is reclaim safe against concurrent call lifecycle?" without standing
// up the SIP plane. Churn refs are DISJOINT from the reclaim targets (and live-map
// only — `create` doesn't write the `pri:` store the scan reads), so the oracle is
// deterministic: every target ends up served, no churn residue, no lock leak.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn churn_during_reclaim_keeps_state_consistent() {
    const TARGETS: usize = 500;
    const WRITERS: usize = 3;
    const PER_WRITER: usize = 400;

    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w0", repl.clone());

    // Seed N reclaim targets into pri:w0 (the partition `reclaim_scan` reads).
    let mut targets = Vec::with_capacity(TARGETS);
    for i in 0..TARGETS {
        let call = build_initial_call(&invite("w0", "w1", &format!("rec-{i}")), src(), &config_for("w0"), 0);
        put(&repl, PRI, "w0", &call).await;
        targets.push(call.call_ref.clone());
    }

    let started = std::time::Instant::now();
    // Reclaim task: the state-level core of `router::reclaim_all` — scan pri:w0,
    // then per-call lock + materialise each into the live serving map.
    let reclaim = {
        let state = state.clone();
        tokio::spawn(async move {
            for (call, _skew) in state.reclaim_scan().await {
                let cr = call.call_ref.clone();
                let _g = state.lock(&cr).await;
                state.materialize_if_absent(call);
            }
        })
    };

    // Churn tasks: full create → update → delete lifecycle on DISJOINT refs, each
    // under the per-call lock (as `router::process` holds it across a handler).
    let mut churn = Vec::with_capacity(WRITERS);
    for w in 0..WRITERS {
        let state = state.clone();
        churn.push(tokio::spawn(async move {
            for j in 0..PER_WRITER {
                let call = build_initial_call(
                    &invite("w0", "w1", &format!("churn-{w}-{j}")),
                    src(),
                    &config_for("w0"),
                    0,
                );
                let cr = call.call_ref.clone();
                let _g = state.lock(&cr).await;
                state.create(call.clone());
                state.update(call);
                state.remove(&cr);
            }
        }));
    }

    reclaim.await.unwrap();
    for c in churn {
        c.await.unwrap();
    }
    let elapsed = started.elapsed();
    let churn_ops = (WRITERS * PER_WRITER * 3) as f64; // create+update+remove each
    eprintln!(
        "\n=== reclaim + concurrent churn (multi_thread) ===\n  \
         reclaim: {:>8.0} calls/s ({TARGETS} materialised)\n  \
         churn:   {:>8.0} ops/s   ({} create/update/delete ops)\n  \
         wall:    {elapsed:?}\n",
        TARGETS as f64 / elapsed.as_secs_f64(),
        churn_ops / elapsed.as_secs_f64(),
        churn_ops as u64,
    );

    // Invariants — must hold regardless of interleaving:
    // 1. Every reclaim target materialised into the live serving map.
    let served = targets.iter().filter(|r| state.peek(r).is_some()).count();
    assert_eq!(served, TARGETS, "all {TARGETS} reclaim targets materialised under churn");
    // 2. No churn residue / no resurrection: only the targets remain live.
    assert_eq!(
        state.active_count(),
        TARGETS,
        "only the {TARGETS} reclaim targets remain live (churn fully created+deleted)",
    );
    // 3. No per-call lock leak: the locks map tracks the live set (the orphan-leak
    //    invariant — a residue here is the ratchet the leak-detector caught).
    assert_eq!(
        state.lock_count(),
        state.active_count(),
        "no per-call lock leak after concurrent churn + reclaim",
    );
}
