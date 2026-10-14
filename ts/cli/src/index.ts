/**
 * **@sip/cli** — the command lines over `@sip/corpus`: flag sets, command
 * builders and the runner that turns a handler's answer into the exit code.
 *
 * A deployment's entry point is one builder and one composition: the builder
 * takes the deployment's {@link CliFacts} (what the parser needs), and the
 * deployment's layer provides every `@sip/corpus` seam the handler reads.
 */
export type { CliFacts, HostEnv } from "./facts.js"
export * as IdFlags from "./id-flags.js"
