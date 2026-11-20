/**
 * A command whose answer is a FILE: its stdout is written straight to
 * `target`, never held by this process, so an answer of any size costs one
 * descriptor.
 *
 * It ANSWERS how it ended rather than failing — a caller running many of
 * these must lose the one, never the batch — and leaves no `target` behind a
 * failure, because the bytes land beside it (`<target>.<pid>.partial`, one
 * writer per process) and are renamed onto it. A signal is how a memory
 * scope's kill arrives, and it is named as one, with what the cgroup
 * recorded of its OOM killer over the run (`oom.ts`).
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"
import * as Oom from "./oom.js"
import { messageOf, said, signalOf } from "./said.js"

/** How one run ended. */
export interface Ended {
  /**
   * Why it never ran, or why its stdout could not be written; `undefined`
   * otherwise, its ending then told by `exit` and `signal`.
   */
  readonly broke: string | undefined
  /** Whether the process ran: `false` where it could not be spawned or awaited, which `broke` then says. */
  readonly started: boolean
  /** The exit code; `null` where a signal ended it or it never ran. */
  readonly exit: number | null
  readonly signal: string | null
  /** What the cgroup recorded of its OOM killer over the run; `null` unless a signal ended it. */
  readonly oom: Oom.Evidence | null
  /** The head of what it wrote on stderr: its first chunks, up to the first past `stderrHead` characters. */
  readonly stderr: string
}

/** How much of stderr is kept when the caller names no head. */
const HEAD = 300

/**
 * `binary args` with stdout into `target` and the head of its stderr kept.
 * `target` is written only where the command exited 0.
 */
export const exec = (
  binary: string,
  args: ReadonlyArray<string>,
  target: string,
  options: { readonly stderrHead?: number } = {}
): Effect.Effect<Ended, never, FileSystem.FileSystem | ChildProcessSpawner.ChildProcessSpawner> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
    const head = options.stderrHead ?? HEAD
    const partial = `${target}.${process.pid}.partial`
    const oomBefore = yield* Oom.count
    const ended = yield* Effect.gen(function*() {
      yield* fs.makeDirectory(path.dirname(target), { recursive: true })
      const handle = yield* spawner.spawn(
        ChildProcess.make(binary, [...args], { stdin: "ignore", killSignal: "SIGTERM" })
      )
      const [written, stderr, exit] = yield* Effect.all(
        [
          handle.stdout.pipe(
            Stream.run(fs.sink(partial)),
            Effect.as(undefined),
            Effect.catch((error) => Effect.succeed(`writing ${target} failed: ${error.message}`))
          ),
          handle.stderr.pipe(
            Stream.decodeText(),
            Stream.runFold(() => "", (heard: string, piece: string) => (heard.length < head ? heard + piece : heard))
          ),
          Effect.result(handle.exitCode)
        ],
        { concurrency: "unbounded" }
      )
      const signal = exit._tag === "Failure" ? signalOf(exit.failure) ?? null : null
      const unawaited = exit._tag === "Failure" && signal === null
      const broke = unawaited ? `${binary} could not run: ${messageOf(exit.failure)}` : written
      return {
        broke,
        started: !unawaited,
        exit: exit._tag === "Success" ? exit.success as number : null,
        signal,
        oom: signal === null ? null : yield* Oom.since(oomBefore),
        stderr
      } satisfies Ended
    }).pipe(
      Effect.scoped,
      Effect.catchCause((cause) =>
        Effect.succeed(
          {
            broke: `${binary} could not be started: ${messageOf(cause)}`,
            started: false,
            exit: null,
            signal: null,
            oom: null,
            stderr: ""
          } satisfies Ended
        )
      )
    )
    if (ended.broke !== undefined || ended.exit !== 0) {
      yield* fs.remove(partial, { force: true }).pipe(Effect.ignore)
      return ended
    }
    return yield* fs.rename(partial, target).pipe(
      Effect.as(ended),
      Effect.catch((error) =>
        fs.remove(partial, { force: true }).pipe(
          Effect.ignore,
          Effect.as({ ...ended, broke: `publishing ${target} failed: ${error.message}` })
        )
      )
    )
  })

/** `binary args` with stdout into `target`; `undefined` where it ran and exited 0, else why not, in one line. */
export const run = (
  binary: string,
  args: ReadonlyArray<string>,
  target: string
): Effect.Effect<string | undefined, never, FileSystem.FileSystem | ChildProcessSpawner.ChildProcessSpawner> =>
  Effect.map(exec(binary, args, target), (ended) => {
    if (ended.broke !== undefined) return ended.broke
    const words = said(ended.stderr).slice(-HEAD)
    if (ended.signal !== null) {
      const oom = ended.oom === null ? "" : ` (${Oom.words(ended.oom)})`
      return `${binary} was killed by ${ended.signal}${oom}: ${words}`
    }
    return ended.exit === 0 ? undefined : `${binary} exited ${ended.exit}: ${words}`
  })

export * as IntoFile from "./into-file.js"
