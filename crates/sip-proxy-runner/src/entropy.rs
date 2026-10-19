//! The identifier generator the proxy keys from the OS RNG at startup. A
//! proxy whose OS RNG does not answer stops before it serves: its
//! identifiers would be guessable.

use sip_txn::IdGen;

/// A generator keyed from the OS RNG; the process exits when there is none.
pub fn keyed_id_gen() -> IdGen {
    IdGen::from_entropy().unwrap_or_else(|e| {
        eprintln!("sip-proxy-runner: {e}");
        std::process::exit(1)
    })
}
