//! The identifier generators a worker keys from the OS RNG at startup. A
//! worker whose OS RNG does not answer refuses the boot: its identifiers
//! would be guessable.

use sip_txn::IdGen;

/// A generator keyed from the OS RNG; `Err` when there is none.
pub(crate) fn keyed_id_gen() -> Result<IdGen, String> {
    IdGen::from_entropy().map_err(|e| e.to_string())
}
