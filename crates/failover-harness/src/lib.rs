//! Test-only harness for **multi-node HA failover** (ADR-0011 X10 tier-2,
//! ADR-0013). Composes, under ONE fake clock: the `scenario-harness` SIP plane,
//! a real load-balancing proxy SUT, two replicating `b2bua` workers over the
//! simulated replication fabric, and a combined SIP + replication report.
//!
//! Layers:
//! - [`harness`] — the [`FailoverHarness`] engine + [`ReplicatedB2buaSut`] +
//!   [`ProxySut`] (moved here from `b2bua-harness`; the canonical 5-step failover
//!   scenario and the fault matrix live in `tests/failover.rs`).
//! - [`scenario`] — the [`CallScenario`] step-list + [`SafePoint`] DSL: a callflow
//!   declares its steps and the quiescent points where an injected crash/recovery
//!   is expected to be *transparent*.
//! - [`oracle`] — the transparency oracle: differential (clean baseline vs
//!   failover-injected) + strict From/To/CSeq invariants.
//! - the `transparent_failover!` matrix macro ([`matrix`]) — expands a
//!   `(scenario × safe-point × fault × recovery)` table into one named
//!   `#[tokio::test]` per legal cell (`tests/transparent_v1.rs`).

pub mod combine;
mod harness;
pub mod oracle;
pub mod runner;
pub mod scenario;

pub use combine::{combine_doc, WorkerAxis};
pub use harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut};
pub use runner::run_cell;
pub use scenario::{Cell, DialogState, Event, Fault, Party, Recovery};

/// Run one matrix cell as a differential transparency check: drive it clean
/// (baseline) and with the failover injected (variant), assert the two external
/// observations are identical, and assert the universal teardown sweep on both
/// runs. This is the body every generated `#[tokio::test]` calls.
pub async fn assert_cell_transparent(cell: Cell) {
    let name = cell.name();
    let (baseline, base_sweep) = run_cell(cell, false).await;
    base_sweep.assert_clean(&format!("{name}/baseline"));
    let (variant, var_sweep) = run_cell(cell, true).await;
    var_sweep.assert_clean(&format!("{name}/variant"));
    oracle::assert_transparent(&name, &baseline, &variant);
}

/// **High-level cluster invariant**: across `nodes`, **exactly one** serves
/// `call_ref` (it is owned by precisely one worker — no ghost duplicate from a
/// failed self-release/handback, and no loss). The anti-leak invariant the
/// reclaim + self-release path must hold. Reads the cluster's vocabulary
/// ([`ReplicatedB2buaSut::serves`]), not partition bodies or repl counters.
pub fn assert_single_owner(nodes: &[&ReplicatedB2buaSut], call_ref: &str) {
    let owners: Vec<&str> = nodes.iter().filter(|n| n.serves(call_ref)).map(|n| n.ordinal()).collect();
    assert_eq!(
        owners.len(),
        1,
        "exactly one node must serve {call_ref}; owners = {owners:?} (0 = lost, >1 = ghost duplicate)",
    );
}

/// Count the CDRs whose `call_ref` matches across **every** node's CDR sink — the
/// cross-cluster "exactly one CDR" tally (`§2` invariant #1). Today only per-node
/// `cdr_records()` exists, so this folds them: `0` = the call's end-event was lost
/// (the backup self-released without a CDR), `2` = double-billed (both nodes
/// discharged). A correct teardown leaves exactly `1` *anywhere* in the cluster.
pub fn total_cdrs_for(nodes: &[&ReplicatedB2buaSut], call_ref: &str) -> usize {
    nodes
        .iter()
        .map(|n| {
            n.cdr_records()
                .into_iter()
                .filter(|r| r.call_ref == call_ref)
                .count()
        })
        .sum()
}

/// **The universal "call terminated on the backup" post-condition** (TODO
/// `FixCallTerminateOnBackup` §2). After the call ends and the cluster settles,
/// *every* cell must hold all four invariants, regardless of which node served the
/// terminal request and whatever the primary's fate:
///
/// 1. **Exactly ONE CDR** for `call_ref` across all nodes (not zero = lost, not
///    two = double-billed) — [`total_cdrs_for`].
/// 2. **The call is OVER** — no node serves it and no node holds a replica body,
///    so no later reboot can resurrect it, and both nodes' per-call memory is
///    clean ([`assert_call_fully_released`], which also asserts 0 serving owners).
/// 3. **Limiter released exactly once** — the shared `LimiterServer`'s
///    `current_total == 0` (not leaked at ≥1, not driven negative by a double
///    release).
///
/// `limiter` is the shared `WindowStore` behind the cluster's one `LimiterServer`;
/// since each cell runs a single call, its global `current_total` is this call's.
pub async fn assert_call_fully_over(
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
    limiter: &call_limiter::WindowStore,
) {
    // #2 — over everywhere (0 owners, no replica trace, memory clean).
    assert_call_fully_released(nodes, call_ref).await;
    // #1 — exactly one CDR across the whole cluster.
    let cdrs = total_cdrs_for(nodes, call_ref);
    assert_eq!(
        cdrs, 1,
        "expected EXACTLY ONE CDR for {call_ref} across the cluster (0 = lost / \
         backup self-released without a CDR, 2 = double-billed); got {cdrs}",
    );
    // #3 — limiter released exactly once (drained to zero, never negative).
    let total = limiter.stats().current_total;
    assert_eq!(
        total, 0,
        "limiter hold for {call_ref} not released exactly once: current_total = {total} \
         ({} = leaked / pinned, {} = double release)",
        if total > 0 { "positive" } else { "" },
        if total < 0 { "negative" } else { "" },
    );
}

/// **High-level cluster invariant**: a terminated call left **no trace anywhere** —
/// no node serves it and no node holds a replica body for it, so a later reboot
/// cannot resurrect it and no per-call memory leaked. Reads
/// [`ReplicatedB2buaSut::holds_any_trace`] + [`ReplicatedB2buaSut::memory_clean`].
pub async fn assert_call_fully_released(nodes: &[&ReplicatedB2buaSut], call_ref: &str) {
    for n in nodes {
        assert!(
            !n.holds_any_trace(call_ref).await,
            "node {} still holds a trace of terminated call {call_ref} (live or replica)",
            n.ordinal(),
        );
        assert!(
            n.memory_clean(),
            "node {} did not clean up per-call memory ({} live, {} locks)",
            n.ordinal(),
            n.active_calls(),
            n.lock_count(),
        );
    }
}

/// Expand a table of `(state, event, fault, recovery, seed)` rows into one named
/// `#[tokio::test]` per cell (ADR-0013). Each test runs the differential
/// transparency check + the universal teardown sweep.
///
/// ```ignore
/// transparent_matrix! {
///     established__bye_alice__kill__stay_dead:
///         Established, Bye(Party::Caller), Kill, StayDead, 0;
/// }
/// ```
#[macro_export]
macro_rules! transparent_matrix {
    ($( $name:ident : $state:expr, $event:expr, $fault:expr, $recovery:expr, $seed:expr ; )+) => {
        $(
            #[tokio::test(start_paused = true)]
            #[allow(non_snake_case)]
            async fn $name() {
                $crate::assert_cell_transparent($crate::Cell {
                    state: $state,
                    event: $event,
                    fault: $fault,
                    recovery: $recovery,
                    seed: $seed,
                })
                .await;
            }
        )+
    };
}

// Re-export the engine value types the failover tests + DSL touch so a consumer
// needs only this crate (+ scenario-harness) for the canonical scenario.
pub use b2bua::store::PartitionRole;
pub use sip_proxy::registry::WorkerHealth;
