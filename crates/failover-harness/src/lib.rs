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
