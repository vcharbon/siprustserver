/**
 * The base's hosted lane: the upstream B2BUA runner (`b2bua-runner`) as the
 * system under test, on the real clock, its cells run by the plain replay
 * binary's batch process (`pivot-replay batch`), each call's b-leg pinned to
 * its cell's egress by the dial (`pivot-replay` `plain_hosted.rs`). The lane
 * serves no backend (`[backend] mode = "none"`): the runner decides each
 * route itself.
 *
 * The lane names a `b2bua-runner` part ({@link RUNNER_PART}): where the
 * runner publishes its metrics (the batch's end reads its active calls
 * there) and where its operator routes listen. A `managed` lane stands one
 * runner per batch on the lane's `[sip] ingress`; a `standing` one is
 * attached to as found. Each file the base has a runner it stands write is a
 * part of its own, `{ file }`, which a managed lane alone may name, and which
 * a plug-in reading that file needs, so a lane naming none never wires it:
 *
 * - `runner-log` ({@link LOG_PART}): the file the runner's output is
 *   appended to, where a collector reading the system's log reads it; a
 *   stamped file under the batch's directory where the lane names none.
 * - `runner-pid` ({@link PID_PART}): the file naming the runner's pid while
 *   it runs, where an operation acting on the runner finds it.
 * - `sip-capture` ({@link CAPTURE_PART}): the runner appends each SIP
 *   datagram to it (`B2BUA_CAPTURE`), where a collector cutting the capture
 *   reads it.
 * - `cdr-file` ({@link CDR_PART}): the runner appends each call's record to
 *   it, a JSON line each (`B2BUA_CDR_FILE`), where a collector of the call
 *   records reads them.
 */
import * as Batch from "@sip/lab-host/replay-batch"
import * as Sut from "@sip/lab-host/replay-sut"
import type { LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import { spawn } from "node:child_process"
import { mkdirSync } from "node:fs"
import * as path from "node:path"

/** The part kind a hosted lane of the base names its runner's addresses in. */
export const RUNNER_PART = "b2bua-runner"

/** The part kind a hosted lane of the base names the file its runner's output is appended to in. */
export const LOG_PART = "runner-log"

/** The part kind a hosted lane of the base names the file naming its runner's pid in. */
export const PID_PART = "runner-pid"

/** The part kind a hosted lane of the base names the file its runner captures its SIP datagrams to in. */
export const CAPTURE_PART = "sip-capture"

/** The part kind a hosted lane of the base names the file its runner appends its call records to in. */
export const CDR_PART = "cdr-file"

/** The variable naming the B2BUA runner a managed lane stands, ahead of any other place. */
export const RUNNER_VARIABLE = "B2BUA_RUNNER_BIN"

/** How long the batch's end line may take once its cells ended: the plain batch's reap bound (10 s) and its margin. */
export const END_WITHIN_MS = 30_000

/** What the base reads of a lane's `b2bua-runner` part. */
export const RunnerPart = Schema.Struct({
  /** Where the runner publishes Prometheus text, `ip:port`. */
  metrics: Schema.String,
  /** Where the runner's operator routes listen, `ip:port`. */
  admin: Schema.String
})
export interface RunnerPart extends Schema.Schema.Type<typeof RunnerPart> {}

/** The `b2bua-runner` part `lane` names, or why the base cannot host it. */
export const runnerPartOf = (lane: LaneView.Hosted): Result.Result<RunnerPart, string> => {
  const part = lane.loaded.parts.get(RUNNER_PART)
  if (part === undefined) return Result.fail(`${lane.id}: a hosted lane of the base names a ${RUNNER_PART} part (its metrics and admin addresses)`)
  return Result.mapError(
    Schema.decodeUnknownResult(RunnerPart)(part.value),
    (issue) => `${lane.id}: its ${RUNNER_PART} part ${part.file}: ${String(issue)}`
  )
}

/**
 * What the base reads of a part naming one file its runner writes: a
 * `runner-log` part's, which the runner's output is appended to; a
 * `runner-pid` part's, naming its pid while it runs; a `sip-capture` part's
 * pcapng file, which the runner appends each SIP datagram it sends and
 * receives to, a section per start; a `cdr-file` part's, which it appends
 * each call's record to, a JSON line each.
 */
export const FilePart = Schema.Struct({ file: Schema.String })
export interface FilePart extends Schema.Schema.Type<typeof FilePart> {}

/** A part naming a file the runner writes: its kind, and what the base does with it, for a refusal. */
interface FileKind {
  readonly kind: string
  readonly doing: string
}

const LOG: FileKind = { kind: LOG_PART, doing: "sets the log file only of a runner it stands" }
const PID: FileKind = { kind: PID_PART, doing: "names the pid only of a runner it stands" }
const CAPTURE: FileKind = { kind: CAPTURE_PART, doing: "captures only a runner it stands" }
const CDR: FileKind = { kind: CDR_PART, doing: "sets the record file only of a runner it stands" }

/** The part of `named`'s kind `lane` names, none where it names none, or why the base cannot set its file: the lane is standing, or the part names none. */
const filePartOf = (lane: LaneView.Hosted, named: FileKind): Result.Result<FilePart | undefined, string> => {
  const part = lane.loaded.parts.get(named.kind)
  if (part === undefined) return Result.succeed(undefined)
  if (lane.sip.lifecycle !== "managed") {
    return Result.fail(`${lane.id}: the base ${named.doing}: a standing lane names no ${named.kind} part (${part.file})`)
  }
  return Result.mapError(
    Schema.decodeUnknownResult(FilePart)(part.value),
    (issue) => `${lane.id}: its ${named.kind} part ${part.file}: ${String(issue)}`
  )
}

/** The `runner-log` part `lane` names, none where it names none, or why the base cannot set it. */
export const logPartOf = (lane: LaneView.Hosted) => filePartOf(lane, LOG)

/** The `runner-pid` part `lane` names, none where it names none, or why the base cannot set it. */
export const pidPartOf = (lane: LaneView.Hosted) => filePartOf(lane, PID)

/** The `sip-capture` part `lane` names, none where it names none, or why the base cannot capture it. */
export const capturePartOf = (lane: LaneView.Hosted) => filePartOf(lane, CAPTURE)

/** The `cdr-file` part `lane` names, none where it names none, or why the base cannot set it. */
export const cdrPartOf = (lane: LaneView.Hosted) => filePartOf(lane, CDR)

/** Why the base cannot host `lane`, or null: it serves no backend, reads the runner's part, and sets the files of a runner it stands alone. */
export const hostedRefusalOf = (lane: LaneView.Hosted): string | null => {
  if (lane.backendMode !== "none") {
    return `${lane.id}: the base serves no backend to its B2BUA runner, which routes each call itself: state [backend] mode = "none"`
  }
  const parts: ReadonlyArray<Result.Result<unknown, string>> = [
    runnerPartOf(lane),
    logPartOf(lane),
    pidPartOf(lane),
    capturePartOf(lane),
    cdrPartOf(lane)
  ]
  for (const part of parts) if (Result.isFailure(part)) return part.failure
  return null
}

/**
 * The B2BUA runner `env` names: {@link RUNNER_VARIABLE}, else `b2bua-runner`
 * in the prebuilt binaries' directory `binDir`, else the release build in
 * `checkout`.
 */
export const runnerBinaryOf = (env: NodeJS.ProcessEnv, binDir: string | undefined, checkout: string): string => {
  const named = env[RUNNER_VARIABLE]
  if (named !== undefined && named !== "") return path.resolve(named)
  if (binDir !== undefined && binDir !== "") return path.join(path.resolve(binDir), "b2bua-runner")
  return path.join(checkout, "target", "release", "b2bua-runner")
}

/**
 * The runner's environment on `lane`: standalone at the lane's ingress, its
 * metrics and admin at the part's addresses, capturing to the file of the
 * lane's `sip-capture` part and appending its call records to the file of
 * its `cdr-file` part where it names them.
 */
export const runnerEnvOf = (lane: LaneView.Hosted, part: RunnerPart): Readonly<Record<string, string>> => {
  const capture = Result.getOrUndefined(capturePartOf(lane))
  const cdr = Result.getOrUndefined(cdrPartOf(lane))
  return {
    B2BUA_MODE: "standalone",
    B2BUA_LISTEN: lane.sip.ingress,
    B2BUA_ADVERTISE: lane.sip.ingress,
    B2BUA_METRICS: part.metrics,
    B2BUA_ADMIN: part.admin,
    ...(capture === undefined ? {} : { B2BUA_CAPTURE: capture.file }),
    ...(cdr === undefined ? {} : { B2BUA_CDR_FILE: cdr.file })
  }
}

/** The variables of `env` naming a file the runner writes, whose directories the base makes before it stands it. */
const WRITTEN = ["B2BUA_CAPTURE", "B2BUA_CDR_FILE"] as const

const partOrFail = (lane: LaneView.Hosted) =>
  Effect.fromResult(Result.mapError(runnerPartOf(lane), (why) => new Error(why)))

/** How the base stands the runner of a managed lane, the binary at `runner`. */
export const sutPackOf = (runner: string): Sut.SutPack<LaneView.View, never> => ({
  worker: "b2bua-runner",
  metricsOf: (lane) => Result.getOrUndefined(Result.map(runnerPartOf(lane), (part) => part.metrics)),
  logOf: (lane) => Result.getOrUndefined(logPartOf(lane))?.file,
  pidFileOf: (lane) => Result.getOrUndefined(pidPartOf(lane))?.file,
  command: (lane, extraEnv) =>
    Effect.gen(function*() {
      const part = yield* partOrFail(lane)
      const env = runnerEnvOf(lane, part)
      for (const variable of WRITTEN) {
        const file = env[variable]
        if (file !== undefined) yield* Effect.try(() => mkdirSync(path.dirname(file), { recursive: true }))
      }
      return ChildProcess.make(runner, [], {
        env: { ...env, ...extraEnv },
        extendEnv: true,
        stdin: "ignore",
        killSignal: "SIGTERM",
        forceKillAfter: "10 seconds"
      })
    })
})

/** The batch-spec's own fields on `lane`: where the batch's end reads the runner's active calls. */
export const wiringOf = (part: RunnerPart) => ({ metrics: part.metrics })

/**
 * The plain replay binary at `replay` as `lane`'s batch process, its scratch
 * under `under`; resolves once it printed its ready line.
 */
export const standBatch = (replay: string, lane: LaneView.Hosted, under: string) =>
  Effect.gen(function*() {
    const part = yield* partOrFail(lane)
    const fs = yield* FileSystem.FileSystem
    yield* fs.makeDirectory(under, { recursive: true })
    const workDir = yield* fs.makeTempDirectory({ directory: under, prefix: "batch-" })
    const specPath = path.join(workDir, "batch-spec.json")
    yield* fs.writeFileString(specPath, JSON.stringify(Batch.batchSpec(lane, wiringOf(part), workDir, undefined), null, 2) + "\n")
    const child = yield* Effect.sync(() => spawn(replay, ["batch", specPath], { stdio: ["pipe", "pipe", "pipe"] }))
    const attached = Batch.attach(child, lane.id, { endWithin: END_WITHIN_MS, servedLine: () => "no backend served" })
    yield* attached.ready
    return attached.host
  })

/** The runner of `lane` stood for a batch, logging where its part says or under `under`. */
export const standSut = (runner: string, lane: LaneView.Hosted, under: string, variables?: Readonly<Record<string, string>>) =>
  Sut.stand(sutPackOf(runner), lane, under, variables)

export * as BaseHostedLane from "./base-hosted-lane.ts"
