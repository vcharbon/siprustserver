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
import { messageOf, said, signalOf } from "./said.js"
import { CliUnavailable } from "./spawn.js"

/** Where the command runs, what it adds to the environment it inherits, and how long a TERM is given. */
export interface LoggedOptions {
  readonly cwd?: string
  readonly env?: Record<string, string>
  readonly forceKillAfter?: Duration.Input
  /** Whether `env` is the whole environment rather than an addition to the inherited one. */
  readonly replaceEnv?: boolean
  /** Append to `file` rather than replace it. */
  readonly append?: boolean
}

const commandOf = (binary: string, args: ReadonlyArray<string>, options: LoggedOptions) =>
  ChildProcess.make(binary, [...args], {
    ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
    ...(options.env === undefined ? {} : { env: options.env, extendEnv: options.replaceEnv !== true }),
    stdin: "ignore",
    killSignal: "SIGTERM",
    ...(options.forceKillAfter === undefined ? {} : { forceKillAfter: options.forceKillAfter })
  })

/** How a run ended: its exit code, or the signal that killed it (an OOM kill is `SIGKILL`). */
export interface Ended {
  /** The exit code; `null` where a signal ended it. */
  readonly exit: number | null
  readonly signal: string | null
}

/**
 * `binary args` with its output in `file`; answers how it ended. A command a
 * signal killed is an answer, never a failure: only one that could not be
 * spawned or read fails.
 */
export const run = Effect.fn("Logged.run")(
  function*(binary: string, args: ReadonlyArray<string>, file: string, options: LoggedOptions = {}) {
    const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
    const fs = yield* FileSystem.FileSystem
    const handle = yield* spawner.spawn(commandOf(binary, args, options))
    const [, exit] = yield* Effect.all(
      [
        handle.all.pipe(Stream.run(fs.sink(file, { flag: options.append === true ? "a" : "w" }))),
        Effect.result(handle.exitCode)
      ],
      { concurrency: "unbounded" }
    )
    if (exit._tag === "Success") return { exit: exit.success as number, signal: null } satisfies Ended
    const signal = signalOf(exit.failure)
    if (signal === undefined) return yield* Effect.fail(exit.failure)
    return { exit: null, signal } satisfies Ended
  },
  (effect, binary, args) =>
    effect.pipe(
      Effect.scoped,
      Effect.catchCause((cause) => Effect.fail(new CliUnavailable({ binary, args: [...args], cause })))
    )
)

/** How one run in a shared log ended: its exit code, or the signal it died of, and its last words. */
export interface Section {
  /** The exit code; `null` where a signal ended it. */
  readonly exit: number | null
  readonly signal: string | null
  /** The end of what it wrote on stderr, stack frames taken out, at most `tail` characters. */
  readonly said: string
}

/** How much of stderr is held to read a section's last words off. */
const HELD = 64 * 1024

/**
 * `binary args` APPENDED to the shared log `file` as one section: a
 * `=== header ===` line, its output interleaved, then `=== exit N ===`. Its
 * last `tail` characters of stderr come back with the code, so a caller can
 * state why it died without reading the log.
 */
export const section = Effect.fn("Logged.section")(
  function*(
    binary: string,
    args: ReadonlyArray<string>,
    file: string,
    options: LoggedOptions & { readonly header: string; readonly tail: number }
  ) {
    const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
    const fs = yield* FileSystem.FileSystem
    const write = (text: string) => fs.writeFileString(file, text, { flag: "a" })
    yield* write(`=== ${options.header} ===\n`)
    const handle = yield* spawner.spawn(commandOf(binary, args, options))
    let heard = ""
    const decoder = new TextDecoder()
    const stderr = handle.stderr.pipe(
      Stream.tap((chunk) =>
        Effect.sync(() => {
          heard = (heard + decoder.decode(chunk, { stream: true })).slice(-HELD)
        })
      )
    )
    const [, exit] = yield* Effect.all(
      [Stream.merge(handle.stdout, stderr).pipe(Stream.run(fs.sink(file, { flag: "a" }))), Effect.result(handle.exitCode)],
      { concurrency: "unbounded" }
    )
    const signal = exit._tag === "Failure"
      ? signalOf(exit.failure) ?? null
      : null
    if (exit._tag === "Failure" && signal === null) return yield* Effect.fail(exit.failure)
    const code = exit._tag === "Success" ? exit.success as number : null
    yield* write(`=== exit ${code ?? signal} ===\n`)
    return { exit: code, signal, said: said(heard).slice(-options.tail) } satisfies Section
  },
  (effect, binary, args) =>
    effect.pipe(
      Effect.scoped,
      Effect.catchCause((cause) => Effect.fail(new CliUnavailable({ binary, args: [...args], cause })))
    )
)

export * as Logged from "./logged.js"
