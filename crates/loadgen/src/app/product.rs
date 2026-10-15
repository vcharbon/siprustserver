//! The product seam: what a load binary plugs into the loadgen application.
//!
//! A product names its binary, adds its own flags beside loadgen's, sets the
//! loadgen flag defaults it runs with, and builds the shape registry the run
//! drives — from compiled shapes, a generator, or authored documents, each a
//! [`ShapeSource`](crate::ShapeSource). [`main`] does everything else (parsing,
//! profile precedence, mux, driver, metrics, report), so a product binary is
//!
//! ```ignore
//! fn main() -> std::io::Result<()> {
//!     loadgen::app::main(my_crate::MyProduct)
//! }
//! ```
//!
//! and the shipped `loadgen` bin is that caller with [`Shipped`].

use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches};
use e2e_model::EndpointConfig;

use super::{run, Args};
use crate::ShapeRegistry;

/// A load product: the shapes a load binary drives and the flags that select
/// them.
pub trait Product {
    /// The product's own flags, parsed beside loadgen's ([`NoFlags`] for none).
    /// Their clap ids must not collide with an [`Args`] flag.
    type Flags: clap::Args;

    /// The binary name `--help` and the log lines show.
    fn name(&self) -> &'static str;

    /// The one-line description `--help` shows.
    fn about(&self) -> &'static str;

    /// Loadgen flag defaults this product runs with, as `(clap id, value)`.
    /// A default changes the starting value only: a `--load-profile` field
    /// still overrides it, and an explicit flag still overrides both.
    fn flag_defaults(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Adjust loadgen's own args from the product flags before the run reads
    /// them (a product mode with its own default correlation, say). `Err`
    /// refuses the command line.
    fn configure(&self, _flags: &Self::Flags, _args: &mut Args) -> Result<(), String> {
        Ok(())
    }

    /// The registry the run's mix resolves against, built once the run's
    /// endpoint layout is resolved and before any socket is bound. `Err`
    /// fails the run.
    fn registry(
        &self,
        flags: &Self::Flags,
        layout: &RunLayout<'_>,
    ) -> Result<ShapeRegistry, String>;
}

/// The resolved endpoint layout a product builds its registry for.
pub struct RunLayout<'a> {
    /// The run's environment-axis document (authored or flag-synthesized).
    pub endpoint: &'a EndpointConfig,
}

impl RunLayout<'_> {
    /// Whether the caller and the callee bind one address, so the SUT returns
    /// each new leg to the address that sent the call.
    pub fn shared_socket(&self) -> bool {
        self.endpoint.addr("alice") == self.endpoint.addr("bob")
    }
}

/// The flags of a product that adds none.
#[derive(clap::Args, Debug, Default)]
pub struct NoFlags {}

/// The shipped product: every default shape ([`ShapeRegistry::with_defaults`]).
pub struct Shipped;

impl Product for Shipped {
    type Flags = NoFlags;

    fn name(&self) -> &'static str {
        "loadgen"
    }

    fn about(&self) -> &'static str {
        "SIP load generator (multiplexed SIPp substitute)"
    }

    fn registry(&self, _: &NoFlags, _: &RunLayout<'_>) -> Result<ShapeRegistry, String> {
        Ok(ShapeRegistry::with_defaults())
    }
}

/// The product's command: loadgen's flags, the product's flags, its name and
/// description, and its flag defaults. Panics when a default names no flag.
pub fn command<P: Product>(product: &P) -> clap::Command {
    let cmd = <P::Flags as clap::Args>::augment_args(Args::command());
    let mut cmd = cmd.name(product.name()).about(product.about());
    for &(id, value) in product.flag_defaults() {
        cmd = cmd.mut_arg(id, |arg| arg.default_value(value));
    }
    cmd
}

/// Parse `itr` (first item = the program name) as `product`'s command line:
/// its flags, and loadgen's args with their explicit-flag record, configured by
/// the product.
pub fn parse_from<P, I, T>(product: &P, itr: I) -> Result<(P::Flags, Args), clap::Error>
where
    P: Product,
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut cmd = command(product);
    let matches = cmd.try_get_matches_from_mut(itr)?;
    let flags = P::Flags::from_arg_matches(&matches)?;
    let mut args = Args::from_matches(&matches)?;
    product
        .configure(&flags, &mut args)
        .map_err(|e| cmd.error(clap::error::ErrorKind::ValueValidation, e))?;
    Ok((flags, args))
}

/// The whole load binary for `product`: parse the process command line
/// (exiting with clap's usage error on a bad one), then run on a
/// multi-threaded runtime.
pub fn main<P: Product>(product: P) -> std::io::Result<()> {
    let (flags, args) = parse_from(&product, std::env::args_os()).unwrap_or_else(|e| e.exit());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(&product, &flags, args))
}

#[cfg(test)]
#[path = "product_tests.rs"]
mod tests;
