//! The identifier generators a worker keys from the OS RNG at startup. A
//! worker whose OS RNG does not answer stops before it serves: its
//! identifiers would be guessable.

use sip_txn::IdGen;

/// A generator keyed from the OS RNG; the process exits when there is none.
pub(crate) fn keyed_id_gen() -> IdGen {
    IdGen::from_entropy().unwrap_or_else(|e| {
        eprintln!("b2bua worker: {e}");
        std::process::exit(1)
    })
}
