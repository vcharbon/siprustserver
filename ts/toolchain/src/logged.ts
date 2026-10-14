/**
 * A command whose output is a FILE: stdout and stderr streamed to `file` as
 * they come, interleaved, never held by this process — an output of any size
 * costs one descriptor.
 *
 * Interruptible like every run here: the process lives in the run's scope, so
 * a fiber interrupted mid-run (a timeout, a stop) SIGTERMs it on the way out,
 * and SIGKILLs it after `forceKillAfter` where the caller states one.
 */
import type * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { CliUnavailable } from "./spawn.js"

/** Where the command runs, what it adds to the environment it inherits, and how long a TERM is given. */
export interface LoggedOptions {
  readonly cwd?: string
  readonly env?: Record<string, string>
  readonly forceKillAfter?: Duration.Input
  /** Append to `file` rather than replace it. */
  readonly append?: boolean
}

/** `binary args` with its output in `file`; answers the exit code. */
export const run = Effect.fn("Logged.run")(
  function*(binary: string, args: ReadonlyArray<string>, file: string, options: LoggedOptions = {}) {
    const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
    const fs = yield* FileSystem.FileSystem
    const handle = yield* spawner.spawn(
      ChildProcess.make(binary, [...args], {
        ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
        ...(options.env === undefined ? {} : { env: options.env, extendEnv: true }),
        stdin: "ignore",
        killSignal: "SIGTERM",
        ...(options.forceKillAfter === undefined ? {} : { forceKillAfter: options.forceKillAfter })
      })
    )
    const [, exitCode] = yield* Effect.all(
      [handle.all.pipe(Stream.run(fs.sink(file, { flag: options.append === true ? "a" : "w" }))), handle.exitCode],
      { concurrency: "unbounded" }
    )
    return exitCode as number
  },
  (effect, binary, args) =>
    effect.pipe(
      Effect.scoped,
      Effect.catchCause((cause) => Effect.fail(new CliUnavailable({ binary, args: [...args], cause })))
    )
)

export * as Logged from "./logged.js"
