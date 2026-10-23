/**
 * The DEPLOYMENT seams: everything the orchestration reads that only a
 * deployment can state. Each is a service whose default states nothing or
 * refuses by name; a deployment substitutes its own with `layerWith`, and its
 * entry points provide one composition of them all.
 *
 * Besides these, a deployment states the store's binding (`@sip/store`
 * `StoreBinding`), its case policy (`@sip/pipeline` `CasePolicy`), and, inside
 * its cell host, its lane presets and routing compiler (`@sip/driver`).
 */
export * as Boxing from "./boxing.js"
export * as CaseLayout from "./case-layout.js"
export * as CellHost from "./cell-host.js"
export * as Extraction from "./extraction.js"
export * as Identity from "./identity.js"
export * as LaneHosting from "./lane-hosting.js"
export * as Launcher from "./launcher.js"
export * as Roots from "./roots.js"
export * as Tools from "./tools.js"
export * as Tracker from "./tracker.js"
