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
 * there), where its operator routes listen, and the file its output is
 * appended to, where the part states one (a collector reading the system's
 * log reads it there). A `managed` lane stands one runner per batch on the
 * lane's `[sip] ingress`; a `standing` one is attached to as found. A
 * managed lane may name a `sip-capture` part ({@link CAPTURE_PART}): its
 * runner appends each SIP datagram to the part's file (`B2BUA_CAPTURE`),
 * where a collector cutting the capture reads it.
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

/** The part kind a hosted lane of the base names the file its runner captures its SIP datagrams to in. */
export const CAPTURE_PART = "sip-capture"

/** The variable naming the B2BUA runner a managed lane stands, ahead of any other place. */
export const RUNNER_VARIABLE = "B2BUA_RUNNER_BIN"

/** How long the batch's end line may take once its cells ended: the plain batch's reap bound (10 s) and its margin. */
export const END_WITHIN_MS = 30_000

/** What the base reads of a lane's `b2bua-runner` part. */
export const RunnerPart = Schema.Struct({
  /** Where the runner publishes Prometheus text, `ip:port`. */
  metrics: Schema.String,
  /** Where the runner's operator routes listen, `ip:port`. */
  admin: Schema.String,
  /** The file the runner's output is appended to; a stamped file under the batch's directory where absent. */
  log: Schema.optionalKey(Schema.String)
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

/** What the base reads of a lane's `sip-capture` part. */
export const CapturePart = Schema.Struct({
  /** The pcapng file the runner appends each SIP datagram it sends and receives to, a section per start. */
  file: Schema.String
})
export interface CapturePart extends Schema.Schema.Type<typeof CapturePart> {}

/** The `sip-capture` part `lane` names, none where it names none, or why the base cannot capture it. */
export const capturePartOf = (lane: LaneView.Hosted): Result.Result<CapturePart | undefined, string> => {
  const part = lane.loaded.parts.get(CAPTURE_PART)
  if (part === undefined) return Result.succeed(undefined)
  if (lane.sip.lifecycle !== "managed") {
    return Result.fail(`${lane.id}: the base captures only a runner it stands: a standing lane names no ${CAPTURE_PART} part (${part.file})`)
  }
  return Result.mapError(
    Schema.decodeUnknownResult(CapturePart)(part.value),
    (issue) => `${lane.id}: its ${CAPTURE_PART} part ${part.file}: ${String(issue)}`
  )
}

/** Why the base cannot host `lane`, or null: it serves no backend, reads the runner's part, and captures only a runner it stands. */
export const hostedRefusalOf = (lane: LaneView.Hosted): string | null => {
  if (lane.backendMode !== "none") {
    return `${lane.id}: the base serves no backend to its B2BUA runner, which routes each call itself: state [backend] mode = "none"`
  }
  const part = runnerPartOf(lane)
  if (Result.isFailure(part)) return part.failure
  const capture = capturePartOf(lane)
  return Result.isFailure(capture) ? capture.failure : null
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
 * lane's `sip-capture` part where it names one.
 */
export const runnerEnvOf = (lane: LaneView.Hosted, part: RunnerPart): Readonly<Record<string, string>> => {
  const capture = Result.getOrUndefined(capturePartOf(lane))
  return {
    B2BUA_MODE: "standalone",
    B2BUA_LISTEN: lane.sip.ingress,
    B2BUA_ADVERTISE: lane.sip.ingress,
    B2BUA_METRICS: part.metrics,
    B2BUA_ADMIN: part.admin,
    ...(capture === undefined ? {} : { B2BUA_CAPTURE: capture.file })
  }
}

const partOrFail = (lane: LaneView.Hosted) =>
  Effect.fromResult(Result.mapError(runnerPartOf(lane), (why) => new Error(why)))

/** How the base stands the runner of a managed lane, the binary at `runner`. */
export const sutPackOf = (runner: string): Sut.SutPack<LaneView.View, never> => ({
  worker: "b2bua-runner",
  metricsOf: (lane) => Result.getOrUndefined(Result.map(runnerPartOf(lane), (part) => part.metrics)),
  logOf: (lane) => Result.getOrUndefined(runnerPartOf(lane))?.log,
  command: (lane, extraEnv) =>
    Effect.gen(function*() {
      const part = yield* partOrFail(lane)
      const env = runnerEnvOf(lane, part)
      const capture = env["B2BUA_CAPTURE"]
      if (capture !== undefined) yield* Effect.try(() => mkdirSync(path.dirname(capture), { recursive: true }))
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
