//! Library face of the standalone B2BUA runner. Exposes the **production
//! callflow-service registry** ([`compose_services`]) so it is single-sourced:
//! the `xtask state-machine-docs` doc generator and the CI freshness test read
//! the same composed list the process runs — including in-tree and separate-crate
//! services (ADR-0016 X5). Empty until a service is retrofitted (slices 7/8).

use b2bua::rules::ServiceDef;

/// The composed production service registry: the callflow services this process
/// runs, in priority order. Separate-crate integrators (e.g. `announcement`,
/// slice 8) are appended here. The doc generator prepends the framework
/// `global-call` machine itself.
pub fn compose_services() -> Vec<ServiceDef> {
    Vec::new()
}

/// The committed state-machine diagrams (ADR-0016): `(machine_id, markdown)`
/// pairs, one per machine, written to `docs/sm/<machine_id>.md`. The framework
/// `global-call` machine is prepended by the renderer. Single source for both
/// the `xtask state-machine-docs` writer and the CI freshness test.
pub fn state_machine_docs() -> Vec<(String, String)> {
    b2bua::rules::render_registry(&compose_services())
}
