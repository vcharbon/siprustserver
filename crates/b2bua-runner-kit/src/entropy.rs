//! The identifier generators a worker keys from the OS RNG at startup, and
//! the per-boot worker identity drawn from one. A worker whose OS RNG does not
//! answer refuses the boot: its identifiers would be guessable.

use sip_txn::IdGen;

/// A generator keyed from the OS RNG; `Err` when there is none.
pub(crate) fn keyed_id_gen() -> Result<IdGen, String> {
    IdGen::from_entropy().map_err(|e| e.to_string())
}

/// This worker's identity (ADR-0055 §6): the one the host states (the pod
/// UID), else a per-boot token drawn from `id_gen`. Compared for equality
/// only, never ordered.
pub(crate) fn worker_identity(service: &str, stated: Option<String>, id_gen: &IdGen) -> String {
    let identity =
        stated.unwrap_or_else(|| format!("boot-{:016x}{:016x}", id_gen.draw(), id_gen.draw()));
    tracing::info!(service, %identity, "worker identity");
    identity
}
