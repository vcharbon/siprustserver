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
    vec![
        b2bua::rules::transfer_service_def(),
        // The out-of-tree announcement service (ADR-0016 slice 8) — depends only
        // on `b2bua-sdk`, injected here as a separate-crate integrator.
        announcement::service(),
    ]
}

/// The committed state-machine diagrams (ADR-0016): `(machine_id, markdown)`
/// pairs, one per machine, written to `docs/sm/<machine_id>.md`. The framework
/// `global-call` machine is prepended by the renderer. Single source for both
/// the `xtask state-machine-docs` writer and the CI freshness test.
pub fn state_machine_docs() -> Vec<(String, String)> {
    b2bua::rules::render_registry(&compose_services())
}

/// The committed rendered view (ADR-0016): one self-contained HTML page drawing
/// every machine as an SVG (Mermaid in-browser), written to `docs/sm/index.html`.
/// Single source for the `xtask state-machine-docs` writer and the freshness test.
pub fn state_machine_docs_html() -> String {
    b2bua::rules::render_registry_html(&compose_services())
}
