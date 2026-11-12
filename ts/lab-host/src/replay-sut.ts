/**
 * The lane's SUT: one process serving every cell of a batch.
 *
 * A lane's `[sip]` states ONE thing about its system under test — where it
 * is, and whether the harness has to stand it up (`lifecycle`). `managed`
 * starts one worker, the command its pack builds, on the target's fixed
 * addresses and stops it with the batch; `standing` attaches to a system that
 * was up before the run and stays up after. Nothing else differs: a cell
 * dials the same ingress and is recorded the same way.
 *
 * A managed worker takes its routing decisions from the batch process's
 * backend (the lane's `backend.listen`), the rest of its wiring as its pack
 * states it. The REAP GATE is the batch process's: at the END of the batch
 * nothing should be left, and a worker still holding a call then is holding
 * one no cell owns. What stays here is whether a managed worker DIED, which
 * reads identical to a leak through an unreachable endpoint and is reported
 * as itself.
 *
 * A worker lives in a scope of its own from its spawn: a stand that fails —
 * not ready in time, exited, interrupted — closes it, so no worker outlives a
 * stand that did not hand it over, and the lane's fixed port is free again.
 */
import type { SutHandle } from "@sip/corpus/replay/batch-protocol"
import type { LaneView } from "@sip/rig"
import { Said } from "@sip/toolchain"
import * as Deferred from "effect/Deferred"
import * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import * as Result from "effect/Result"
import * as Scope from "effect/Scope"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

/** How long a managed worker gets to announce itself. */
const READY_WITHIN = Duration.seconds(20)

/** The line the worker prints once it holds its SIP port. */
export const READY = "listening UDP"

/** How long a stopped worker gets to exit on its SIGTERM before it is SIGKILLed. */
export const STOP_GRACE = Duration.seconds(10)

/** How long an exited worker's output gets to reach its log. */
const LOG_DRAIN = Duration.seconds(2)

/** What a worker reaches for. */
export type Requirements = ChildProcessSpawner.ChildProcessSpawner | FileSystem.FileSystem | Path.Path

/** A worker that announced itself. */
export interface Worker {
  /** How it died, if it has; `undefined` while it runs or once stopped. */
  readonly died: () => string | undefined
  /** SIGTERM, SIGKILL past the grace; resolves once it exited. */
  readonly stop: Effect.Effect<void>
}

/** Whether the output seen so far holds {@link READY}, a line split across two chunks included. */
const readiness = () => {
  const decoder = new TextDecoder()
  let tail = ""
  return (chunk: Uint8Array): boolean => {
    const text = tail + decoder.decode(chunk, { stream: true })
    tail = text.slice(-(READY.length - 1))
    return text.includes(READY)
  }
}

/** How an exit code read ended: the code, or the signal it died of. */
const endedBy = (ended: Result.Result<ChildProcessSpawner.ExitCode, unknown>): string =>
  Result.isSuccess(ended) ? String(ended.success) : Said.signalOf(ended.failure) ?? "abnormally"

/** The worker's output all in its log: the log fiber `logged` done, {@link LOG_DRAIN} at most. */
const logClosed = (logged: Fiber.Fiber<unknown, unknown>): Effect.Effect<void> =>
  Fiber.await(logged).pipe(Effect.timeoutOption(LOG_DRAIN), Effect.asVoid)

/**
 * Adds to `scope` the finalizer that runs `stop`, then waits `drained`. Added
 * after the log fiber was forked into `scope`, it runs before that fiber is
 * interrupted (a scope's finalizers run last-added first), so the worker's
 * last output reaches its log.
 */
const stopsBeforeLogEnds = (scope: Scope.Closeable, stop: Effect.Effect<unknown, unknown>, drained: Effect.Effect<void>) =>
  Scope.addFinalizer(scope, stop.pipe(Effect.ignore, Effect.andThen(drained)))

/**
 * `command` started with its output appended to `log`, handed over once it
 * printed {@link READY}. It fails when the worker exits first or stays silent
 * past `readyWithin`, and the worker is gone by the time it does.
 */
export const worker = Effect.fn("ReplaySut.worker")(function*(
  command: ChildProcess.Command,
  log: string,
  name: string,
  readyWithin: Duration.Input = READY_WITHIN
) {
  const spawner = yield* ChildProcessSpawner.ChildProcessSpawner
  const fs = yield* FileSystem.FileSystem
  const scope = yield* Scope.make()
  const stop = Scope.close(scope, Exit.void)
  let died: string | undefined
  yield* Effect.gen(function*() {
    const handle = yield* spawner.spawn(command).pipe(Scope.provide(scope))
    const ready = yield* Deferred.make<void, Error>()
    const seen = readiness()
    const logged = yield* handle.all.pipe(
      Stream.tap((chunk) => (seen(chunk) ? Deferred.succeed(ready, undefined) : Effect.void)),
      Stream.run(fs.sink(log, { flag: "a" })),
      Effect.ignore,
      Effect.forkIn(scope)
    )
    const drained = logClosed(logged)
    yield* stopsBeforeLogEnds(scope, handle.kill({ forceKillAfter: STOP_GRACE }), drained)
    yield* Effect.result(handle.exitCode).pipe(
      Effect.tap(() => drained),
      Effect.flatMap((ended) => {
        died = `the ${name} worker exited ${endedBy(ended)} — its log is ${log}`
        return Deferred.fail(ready, new Error(`the ${name} worker exited ${endedBy(ended)} before it was ready`))
      }),
      Effect.forkIn(scope)
    )
    yield* Deferred.await(ready).pipe(
      Effect.timeoutOrElse({
        duration: readyWithin,
        orElse: () => Effect.fail(new Error(`the ${name} worker never took its port — see ${log}`))
      })
    )
  }).pipe(Effect.onError(() => stop))
  return { died: () => died, stop } satisfies Worker
})

/** What a pack stands its managed worker with. */
export interface SutPack<L extends LaneView.View, R> {
  /** The worker's name in the printed line. */
  readonly worker: string
  /** The SUT's metrics endpoint its handle carries, where the lane's parts state one. */
  readonly metricsOf: (lane: Extract<L, { readonly kind: "hosted" }>) => string | undefined
  /**
   * The managed worker's command line on `lane`, `extraEnv` added to its
   * environment; a lane that cannot wire one fails, naming why.
   */
  readonly command: (
    lane: Extract<L, { readonly kind: "hosted" }>,
    extraEnv: Readonly<Record<string, string>>
  ) => Effect.Effect<ChildProcess.Command, Error, R>
}

/**
 * Where a managed worker logs under `under`: a thousand cells through one
 * worker is a thousand calls' worth of lines, so they go to a file rather than
 * over the batch's own table, and its path is printed, the first thing a red
 * cell wants.
 */
const logUnder = (path: Path.Path, under: string): string =>
  path.join(under, `sut-${new Date().toISOString().replace(/[:.]/g, "-")}.log`)

/**
 * The lane's SUT, stood up where the harness owns it. `undefined` where the
 * lane has none of its own — the simulated lane builds its SUT inside the
 * cell. A managed worker's stop resolves once it exited, so the next batch
 * finds its addresses free. `extraEnv` is added to a managed worker's
 * environment.
 */
export const stand = <L extends LaneView.View, R>(
  pack: SutPack<L, R>,
  found: L,
  under: string,
  extraEnv: Readonly<Record<string, string>> = {}
) =>
  Effect.gen(function*() {
    if (found.kind === "simulated") return undefined
    const hosted = found as Extract<L, { readonly kind: "hosted" }>
    const lane = hosted.id
    const metrics = pack.metricsOf(hosted)
    if (hosted.sip.lifecycle === "standing") {
      return { metrics, died: () => undefined, stop: Effect.void } satisfies SutHandle
    }
    const command = yield* pack.command(hosted, extraEnv)
    const fs = yield* FileSystem.FileSystem
    const path = yield* Path.Path
    yield* fs.makeDirectory(under, { recursive: true }).pipe(Effect.orDie)
    const log = logUnder(path, under)
    const held = yield* worker(command, log, lane)
    console.log(`[sut] ${lane}: one ${pack.worker} on ${hosted.sip.ingress}, logging to ${log}`)
    return { metrics, died: held.died, stop: held.stop } satisfies SutHandle
  }).pipe(Effect.withSpan("ReplaySut.stand"))
