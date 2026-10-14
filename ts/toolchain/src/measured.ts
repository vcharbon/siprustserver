/**
 * A command MEASURED under a usage wrapper: `wrapper --out <usage.json> --
 * command args`, its stdout and stderr appended to a log as they come (and
 * echoed to this process's own where asked), its figures read back from the
 * usage file the wrapper writes, `{ wallMs, maxRssKb, exit }`.
 *
 * Interrupting the measurement SIGTERMs the wrapper's process group and waits
 * for it to end, so no measured command outlives the fiber that started it and
 * the usage file still records how it ended.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import * as Schema from "effect/Schema"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as os from "node:os"
import { signalOf } from "./said.js"

export interface Request {
  /** The wrapper that records the command's usage (`--out <file> -- <command…>`). */
  readonly wrapper: string
  readonly command: string
  readonly args: ReadonlyArray<string>
  readonly cwd: string
  /** Where the output is kept; the usage lands beside it as `<log>.usage.json`. */
  readonly log: string
  /** Whether the output is echoed to this process's own stdout and stderr too. */
  readonly echo: boolean
}

export interface Measured {
  readonly wallMs: number
  /** Null where the command never started or the wrapper died before recording. */
  readonly maxRssKb: number | null
  /** The exit as a shell states it: the code, or 128 plus the number of the signal that ended it. */
  readonly exit: number
  /** The 1-minute load average when the command started. */
  readonly loadAtStart: number
}

const Usage = Schema.Struct({
  wallMs: Schema.Number,
  maxRssKb: Schema.NullOr(Schema.Number),
  exit: Schema.Number
})

const decodeUsage = Schema.decodeUnknownOption(Schema.fromJsonString(Usage))

/** A death by `signal` as a shell states it. */
const signalExit = (signal: string): number =>
  128 + (os.constants.signals[signal as NodeJS.Signals] ?? 0)

/** Where the bytes of one stream go besides the log. */
const echoed = (stream: Stream.Stream<Uint8Array, unknown>, to: NodeJS.WriteStream | undefined) =>
  to === undefined ? stream : stream.pipe(Stream.tap((chunk) => Effect.sync(() => void to.write(chunk))))

/** The command, measured; a wrapper that cannot start answers exit 127 and no figures. */
export const run = Effect.fn("Measured.run")(function*(request: Request) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
  const usageFile = `${request.log}.usage.json`
  yield* fs.makeDirectory(path.dirname(request.log), { recursive: true })
  yield* fs.remove(usageFile, { force: true })
  const loadAtStart = os.loadavg()[0] ?? 0
  const started = performance.now()
  const ended = yield* Effect.scoped(Effect.gen(function*() {
    const handle = yield* spawner.spawn(ChildProcess.make(
      request.wrapper,
      ["--out", usageFile, "--", request.command, ...request.args],
      { cwd: request.cwd, stdin: "ignore", killSignal: "SIGTERM" }
    ))
    const output = Stream.merge(
      echoed(handle.stdout, request.echo ? process.stdout : undefined),
      echoed(handle.stderr, request.echo ? process.stderr : undefined)
    )
    const [, exit] = yield* Effect.all(
      [output.pipe(Stream.run(fs.sink(request.log, { flag: "a" }))), Effect.result(handle.exitCode)],
      { concurrency: "unbounded" }
    )
    if (exit._tag === "Success") return exit.success as number
    const signal = signalOf(exit.failure)
    return signal === undefined ? 1 : signalExit(signal)
  })).pipe(
    Effect.catchCause((cause) =>
      fs.writeFileString(request.log, `measure: ${request.wrapper} could not run: ${String(cause)}\n`, { flag: "a" }).pipe(
        Effect.ignore,
        Effect.as(127)
      )
    )
  )
  const usage = yield* fs.readFileString(usageFile).pipe(
    Effect.map(decodeUsage),
    Effect.orElseSucceed(() => undefined)
  )
  const recorded = usage?._tag === "Some" ? usage.value : undefined
  return {
    wallMs: recorded?.wallMs ?? Math.round(performance.now() - started),
    maxRssKb: recorded?.maxRssKb ?? null,
    exit: recorded?.exit ?? ended,
    loadAtStart
  } satisfies Measured
})

export * as Measured from "./measured.js"
