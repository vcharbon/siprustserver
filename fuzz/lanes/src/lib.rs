//! What the fuzz targets share: the drivers and seeds of the no-panic lanes
//! (ADR-0051 §8), compiled in from the crates' own test trees so a target
//! drives exactly what its lane drives; each target's input layout; the
//! allocation cap every target runs under; and the lane's small stack.

pub mod alloc_cap;
pub mod frame_lane;
pub mod input;
pub mod message_lane;
pub mod proxy_lane;
pub mod turn_lane;
pub mod unheld_lane;

#[global_allocator]
static ALLOC: alloc_cap::CappedAlloc = alloc_cap::CappedAlloc;

/// Runs `f` on a thread with the lanes' stack, so a recursion the input
/// drives overflows here as it would in the lane; a panic is re-raised.
pub fn on_lane_stack(f: impl FnOnce() + Send + 'static) {
    wire_mutations::lane::on_lane_stack(f);
}
