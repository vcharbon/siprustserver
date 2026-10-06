/**
 * **@sip/driver** — the campaign orchestrator.
 *
 * A campaign document lists cells of two shapes, they run into one run
 * directory, and `campaign.json` is what a reader (or the next tool) consumes:
 *
 * ```text
 * <run-dir>/
 *   campaign.json                 the aggregate index
 *   specs/<cell>.run-spec.json    what the driver asked the interpreter for
 *   <cell>/                       the interpreter's out_dir, or a crate's cell dir
 * ```
 *
 * Nothing here knows a lane or a numbering plan. A deployment's own driver is a
 * Layer composition around this one:
 *
 * ```ts
 * import { NodeServices } from "@effect/platform-node"
 * import { LanePresets, RoutingCompiler, driver } from "@sip/driver"
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
export {
  exitCodeOf,
  runCampaign,
  summarize,
  type CampaignOptions,
  type CampaignRun
} from "./campaign.js"
export { cargoTest, INTERRUPTED, runCell, type CellContext, type CellRun, type TestInvocation } from "./cells.js"
export { defaultTs, driver, run } from "./cli.js"
export { LanePresets, LaneUnknown } from "./lanes.js"
export * as Layout from "./layout.js"
export { RoutingCompiler, RoutingRefused } from "./routing.js"
export { emitRunSpec, type LaneBlock, type RunSpec } from "./run-spec.js"
