/**
 * **@sip/cli** — the command lines over `@sip/corpus`: flag sets, command
 * builders and the runner that turns a handler's answer into the exit code.
 *
 * A deployment's entry point is one builder and one composition: the builder
 * takes the deployment's {@link CliFacts} (what the parser needs, and the
 * words its lines are printed in), and the deployment's layer provides every
 * `@sip/corpus` seam the handler reads, beneath the store the line's roots
 * name (`Store.layer`):
 *
 * ```ts
 * import { Commands, Runner, Store, Options } from "@sip/cli"
 *
 * Runner.runMain(FACTS, Commands.Cut.command(FACTS), (input) =>
 *   Store.layer(Options.overridesOf(input), deployment))
 * ```
 */
export * as Commands from "./commands/index.js"
export type { CliFacts, HostEnv as HostEnvFacts } from "./facts.js"
export * as Flags from "./flags.js"
export * as HostEnv from "./host-env.js"
export * as IdFlags from "./id-flags.js"
export * as Options from "./options.js"
export * as Refusals from "./refusals.js"
export * as Runner from "./runner.js"
export * as Store from "./store.js"
