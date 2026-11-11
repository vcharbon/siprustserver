/**
 * **@sip/driver** — running one cell of a campaign matrix.
 *
 * A cell of either shape (a pivot replay, a crate test) runs into the run
 * directory its caller hands it, and answers the `campaign.json` line its
 * caller folds into the index:
 *
 * ```text
 * <run-dir>/
 *   campaign.json                 the aggregate index, the caller's
 *   specs/<cell>.run-spec.json    what the driver asked the interpreter for
 *   <cell>/                       the interpreter's out_dir, or a crate's cell dir
 * ```
 *
 * Nothing here knows a lane or a numbering plan. A deployment states them as
 * layers around {@link runCell}:
 *
 * ```ts
 * import { NodeServices } from "@effect/platform-node"
 * import { LanePresets, RoutingCompiler } from "@sip/driver"
 * import { Reclassifier } from "@sip/pipeline"
 * import { ReplayCli } from "@sip/toolchain"
 * import { Layer } from "effect"
 *
 * const services = Layer.mergeAll(
 *   ReplayCli.layer,
 *   Reclassifier.layer,
 *   LanePresets.layerWith(myLanes),
 *   RoutingCompiler.layerWith(myCompiler)
 * ).pipe(Layer.provide(NodeServices.layer))
 * ```
 */
export { cargoTest, INTERRUPTED, runCell, type CellContext, type CellRun, type TestInvocation } from "./cells.js"
export { LanePresets, LaneUnknown } from "./lanes.js"
export * as Layout from "./layout.js"
export { RoutingCompiler, RoutingRefused } from "./routing.js"
export { emitRunSpec, type LaneBlock, type RunSpec } from "./run-spec.js"
