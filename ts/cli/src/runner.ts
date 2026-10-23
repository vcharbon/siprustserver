/**
 * The RUNNER every entry point ends with: the host's env files applied, the
 * line parsed, the handler's services provided from the layers built for it,
 * the answer made the process exit code.
 *
 * A handler answers with a NUMBER and never fails on a refusal: the command did
 * what it was asked and the verdict rides the exit code, exactly as a usage
 * error rides 2 and `--help` rides 0.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { CliError, Command } from "effect/unstable/cli"
import type { CliFacts } from "./facts.js"
import * as HostEnv from "./host-env.js"

const VERSION = "0.0.0"

/** The handler's answer as the process exit code. */
export const exit = (code: number): Effect.Effect<void> =>
  Effect.sync(() => {
    process.exitCode = code
  })

/**
 * The OPERATOR's directory, which a relative path resolves against: where the
 * package runner was invoked (`INIT_CWD`), else the process's own.
 */
export const operatorCwd = (env: NodeJS.ProcessEnv = process.env): string => env.INIT_CWD ?? process.cwd()

/** What this process was handed on stdin, whole: a path list given as `-`. */
export const stdinText = Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readFileString("/dev/stdin"))

/**
 * One command, run: the host's env files applied (defaults, never overrides),
 * its flags parsed off `process.argv`, its services provided from `layers` and
 * the platform, its exit code set. The layers are built for the handler alone,
 * from the parsed line, so `--help` and a usage error need nothing the layers
 * reach for — the store included — and a root stated on the line is the one
 * the store's services are built over. What the env files did is the
 * handler's to read (`HostEnv.Applied`).
 *
 * `--help` is answered by the parser itself and exits 0; a parse the command
 * refused is rendered by the parser and exits 2.
 */
export const runMain = <Name extends string, Input, E, LA, LE>(
  facts: CliFacts,
  command: Command.Command<Name, Input, unknown, E, LA | NodeServices.NodeServices>,
  layers:
    | Layer.Layer<LA, LE, NodeServices.NodeServices>
    | ((input: Input) => Layer.Layer<LA, LE, NodeServices.NodeServices>)
): void => {
  Effect.gen(function*() {
    const applied = yield* HostEnv.apply(facts.hostEnv)
    yield* Command.run(
      Command.provide(
        command,
        (input: Input): Layer.Layer<LA | NodeServices.NodeServices, LE> =>
          Layer.provideMerge(typeof layers === "function" ? layers(input) : layers, NodeServices.layer)
      ),
      { version: VERSION }
    ).pipe(Effect.provideService(HostEnv.Applied, applied))
  }).pipe(
    Effect.catchIf(CliError.isCliError, () => exit(2)),
    Effect.provide(NodeServices.layer),
    NodeRuntime.runMain
  )
}
