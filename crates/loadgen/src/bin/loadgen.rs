//! `loadgen` CLI — drive the load from the host against the SUT, multiplexing all
//! dialogs over a few static endpoint sockets.
//!
//! Example (host → kind VIP, our b2bua routing the b-leg to the static uas addr):
//! ```text
//! cargo run -p loadgen --release -- \
//!   --cps 50 --duration 600 --max-in-flight 4000 \
//!   --target 172.20.255.250:5060 --bind-ip 172.20.0.1 \
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

fn main() -> std::io::Result<()> {
    loadgen::app::main(loadgen::app::Shipped)
}
