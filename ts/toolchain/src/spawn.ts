/**
 * The subprocess boundary: run one Rust CLI, collect what it wrote and the code
 * it exited with.
 *
 * Every adapter in this package goes through here, so the two facts a caller
 * needs — the bytes and the exit code — arrive together. A CLI that exits
 * non-zero with a valid document on stdout (`pivot-schema lint --json` does, on
 * an error-severity finding) is DATA, not a failure, and only a caller that
 * knows its CLI's vocabulary can say which; {@link Runner.ok} is the shortcut
 * for the ones where any non-zero is a failure.
 *
 * The spawner is bound ONCE, in {@link runner}, so a service layer acquires it
 * at construction and its methods carry no residual requirement.
 *
 * stdout and stderr are drained concurrently with the wait on the exit code: a
 * flows document is megabytes, and a pipe nobody reads is a process that never
 * exits.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

/** What one CLI invocation produced. */
export interface Output {
  readonly stdout: string
  readonly stderr: string
  readonly exitCode: number
}

/**
 * Where a command runs and what it can read out of its environment. Both are
 * spawn concerns, so they are stated here rather than by a caller reaching for
 * its own spawner.
 *
 * `env` EXTENDS the parent environment: a Rust toolchain that lost `PATH` would
 * fail for a reason that has nothing to do with the work.
 */
export interface RunOptions {
  readonly cwd?: string
  readonly env?: Record<string, string>
}

/** The process could not be started, or its pipes failed under it. */
export class CliUnavailable extends Schema.TaggedError<CliUnavailable>()("Toolchain.CliUnavailable", {
  binary: Schema.String,
  args: Schema.Array(Schema.String),
  cause: Schema.Defect()
}) {}

/** The process ran and refused the work. `stderr` is its own words. */
export class CliFailed extends Schema.TaggedError<CliFailed>()("Toolchain.CliFailed", {
  binary: Schema.String,
  args: Schema.Array(Schema.String),
  exitCode: Schema.Int,
  stdout: Schema.String,
  stderr: Schema.String
}) {}

export interface Runner {
  /** Run `binary args`; hand back what it wrote and the code it exited with. */
  readonly all: (
    binary: string,
    args: ReadonlyArray<string>,
    options?: RunOptions
  ) => Effect.Effect<Output, CliUnavailable>
  /** The same, with any non-zero exit turned into a typed failure and stdout returned. */
  readonly ok: (
    binary: string,
    args: ReadonlyArray<string>,
    options?: RunOptions
  ) => Effect.Effect<string, CliUnavailable | CliFailed>
}

/** Bind the platform's spawner into a runner a service layer can hold. */
export const runner: Effect.Effect<Runner, never, ChildProcessSpawner.ChildProcessSpawner> = Effect.gen(
  function* () {
    const spawner = yield* ChildProcessSpawner.ChildProcessSpawner

    const all = Effect.fn("Toolchain.run")(
      function* (binary: string, args: ReadonlyArray<string>, options?: RunOptions) {
        const handle = yield* spawner.spawn(
          ChildProcess.make(binary, [...args], {
            ...(options?.cwd === undefined ? {} : { cwd: options.cwd }),
            ...(options?.env === undefined ? {} : { env: options.env, extendEnv: true })
          })
        )
        const [stdout, stderr, exitCode] = yield* Effect.all(
          [
            handle.stdout.pipe(Stream.decodeText(), Stream.mkString),
            handle.stderr.pipe(Stream.decodeText(), Stream.mkString),
            handle.exitCode
          ],
          { concurrency: "unbounded" }
        )
        return { stdout, stderr, exitCode: exitCode as number } satisfies Output
      },
      (effect, binary, args, _options) =>
        effect.pipe(
          Effect.scoped,
          Effect.catchCause((cause) => new CliUnavailable({ binary, args: [...args], cause }))
        )
    )

    const ok = Effect.fn("Toolchain.runOk")(function* (
      binary: string,
      args: ReadonlyArray<string>,
      options?: RunOptions
    ) {
      const output = yield* all(binary, args, options)
      if (output.exitCode !== 0) {
        return yield* new CliFailed({
          binary,
          args: [...args],
          exitCode: output.exitCode,
          stdout: output.stdout,
          stderr: output.stderr
        })
      }
      return output.stdout
    })

    return { all, ok } satisfies Runner
  }
)

/** The failure a command refused with, from an output the caller decided to reject. */
export const refused = (binary: string, args: ReadonlyArray<string>, output: Output): CliFailed =>
  new CliFailed({
    binary,
    args: [...args],
    exitCode: output.exitCode,
    stdout: output.stdout,
    stderr: output.stderr
  })
