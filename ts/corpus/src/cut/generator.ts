/**
 * How the deployment's case GENERATOR runs for a cut: its command line
 * (`Tools.generator`) inside the cut's memory box (`Boxing`), its V8 heap
 * sized to that box unless the caller sized it (`@sip/toolchain` `NodeHeap`),
 * its output appended to one log as a section.
 *
 * A generator that never ran answers like one that died: the run's last words
 * say why, so the captures it was given carry a reason.
 */
import { Logged, NodeHeap } from "@sip/toolchain"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { Boxing, Tools } from "../deployment/index.js"

/** The generator's log, in the directory of the cut it ran for. */
export const GENERATOR_LOG = "generator.log"

/** The CPUs one generator is given: it is one node process with a little help. */
const CPUS = 4

/** How much of a dead generator's stderr its captures are told. */
const TAIL = 600

/** One generator run: its arguments, the box it is given, and the log it is appended to. */
export interface Launch {
  readonly args: ReadonlyArray<string>
  readonly memoryMax: string
  readonly log: string
}

/** The generator run once; how it ended, with its last words. */
export const run = Effect.fn("Generator.run")(function*(launch: Launch) {
  const generator = yield* Effect.flatMap(Tools.Service, (tools) => tools.generator)
  const boxing = yield* Boxing.Service
  const [command, ...rest] = generator.argv
  const boxed = boxing.box({ memoryMax: launch.memoryMax, cpus: CPUS, command: command!, args: [...rest, ...launch.args] })
  const heap = yield* Effect.sync(() => NodeHeap.environment(launch.memoryMax, process.env))
  if (heap.note !== undefined) yield* Console.log(`[cut] generator heap: ${heap.note}`)
  return yield* Logged.section(boxed.command, boxed.args, launch.log, {
    cwd: generator.cwd,
    env: heap.env,
    replaceEnv: true,
    header: "generator",
    tail: TAIL
  }).pipe(
    Effect.catch((error) =>
      Effect.succeed(
        { exit: null, signal: null, said: `spawn failed: ${String(error.cause)}`.slice(-TAIL) } satisfies Logged.Section
      )
    )
  )
})
