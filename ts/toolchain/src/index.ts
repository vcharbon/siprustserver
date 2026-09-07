/**
 * **@sip/toolchain** — typed adapters over the Rust CLIs.
 *
 * The ONLY package that knows subprocesses exist. Everything above it asks a
 * service for a decoded document or a typed outcome and never learns which
 * binary answered; everything below it is `@sip/contracts`, which decodes what
 * comes back.
 *
 * Each service is a `Layer` over `ChildProcessSpawner`, so a run wires the Node
 * implementation once:
 *
 * ```ts
 * import { NodeServices } from "@effect/platform-node"
 * import { Layer } from "effect"
 * import { PivotSchemaCli, SipflowCli } from "@sip/toolchain"
 *
 * const toolchain = Layer.mergeAll(SipflowCli.layer, PivotSchemaCli.layer).pipe(
 *   Layer.provide(NodeServices.layer)
 * )
 * ```
 */
export * as Binaries from "./binaries.js"
export { PivotSchemaCli } from "./pivot-schema-cli.js"
export { PivotSchemaSchedules } from "./pivot-schema-schedules.js"
export { ReplayCli } from "./replay-cli.js"
export { SipflowCli } from "./sipflow-cli.js"
export {
  CliFailed,
  CliUnavailable,
  type Output,
  refused,
  type Runner,
  runner,
  type RunOptions
} from "./spawn.js"
