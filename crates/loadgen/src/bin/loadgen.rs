//! `loadgen` CLI — drive the load from the host against the SUT, multiplexing all
//! dialogs over a few static endpoint sockets.
//!
//! Example (host → kind VIP, our b2bua routing the b-leg to the static uas addr):
//! ```text
//! cargo run -p loadgen --release -- \
//!   --cps 50 --duration 600 --max-in-flight 4000 \
//!   --target 192.168.60.250:5060 --bind-ip 192.168.60.1 \
//!   --correlation-header X-Loadgen-Id --route-pin-to-uas \
//!   --out-dir ./loadgen-report
//! ```
//!
//! See `crates/loadgen/README.md` for the quick start, how it relates to the
//! existing test suites, and how to add scenarios.
//!
//! The whole application lives in [`loadgen::app`] behind the
//! [`Product`](loadgen::app::Product) seam: this bin is the shipped product,
//! and another product's load bin is the same one line over its own product.

/// jemalloc, not the system allocator: the per-call churn of a load run spends
/// a large share of its CPU in the allocator, and jemalloc's thread caches cut it.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> std::io::Result<()> {
    // The resolved allocator configuration, with the defaults jemalloc-stats
    // sets (ADR-0038).
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();
    loadgen::app::main(loadgen::app::Shipped)
}
