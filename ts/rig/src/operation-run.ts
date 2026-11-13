/**
 * The run of one operation instance (ADR-0060 appendix §7, with the rules of
 * ADR-0049 §2): `inject`, then, whatever happened since, `heal` and its
 * `probe`. Each step is announced and concluded by an {@link Event}:
 * `inject`, then `injected` or `failed`; `heal`, then `healed` or `failed`;
 * `recovered` once the probe answered 0 (right after `healed` where the
 * operation has no probe), `failed` where it answered anything but 0 or 75 or
 * ran out of its bound. A step concludes only from a positive answer.
 *
 * A heal attempt is `heal` and its probe. A failed attempt is run once more;
 * a second failure is the event `lab-left-changed`.
 */
import * as Cause from "effect/Cause"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import * as path from "node:path"
import * as Context from "./context.js"
import * as Executable from "./executable.js"
import type * as Lanes from "./lanes.js"
import * as Operation from "./operation.js"
import type * as RigPath from "./rig-path.js"

/** What an event says of a step. */
export const PHASES = ["inject", "injected", "heal", "healed", "recovered", "failed", "lab-left-changed"] as const
export type Phase = (typeof PHASES)[number]

/** The phase of the event that marks a job whose lab an operation left changed. */
export const LAB_LEFT_CHANGED: Phase = "lab-left-changed"

/** How many heal attempts run before the lab is said left changed. */
export const HEAL_ATTEMPTS = 2

/** One phase of one operation instance, at its instant on the runner's clock and on the wall clock. */
export interface Event {
  /** The instance's label: its operation's name, and what it runs for where that is not the job alone. */
  readonly operation: string
  readonly phase: Phase
  /** Milliseconds on the runner's clock (the lane's own on a real lane). */
  readonly clock_ms: number
  /** Unix milliseconds. */
  readonly wall_ms: number
  /** What failed, and the log that says more; absent on a positive answer. */
  readonly message?: string
}

/** Where the events of a run go: a job's record, a log. It never fails the run. */
export type Sink = (event: Event) => Effect.Effect<void>

/** One operation as it runs for one scope. */
export interface Instance {
  readonly operation: RigPath.Defined<Operation.Operation>
  /** What its events name it by. */
  readonly label: string
  /** The lane whose parts it acts on; none for an operation that needs no part. */
  readonly lane: Lanes.Loaded | undefined
  /** Its own directory: `state/` and `out/`, its context file, a log per step. */
  readonly dir: string
  /** The cell it runs as a step of: its context's scope is then the cell's, the cell its one planned cell. */
  readonly cell?: Context.Planned
  /** The lane binding's parameters, over its manifest's. */
  readonly params?: Readonly<Record<string, unknown>>
}

/** Why an `inject` did not do its work. */
export class InjectFailed extends Schema.TaggedError<InjectFailed>()("Rig.InjectFailed", {
  operation: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.operation}: inject ${this.reason}`
  }
}

/** The directories and the context file of `instance`. */
const layoutOf = (instance: Instance) => ({
  state: path.join(instance.dir, "state"),
  out: path.join(instance.dir, "out"),
  context: path.join(instance.dir, "context.json")
})

/**
 * Lays out `instance`'s directory for `step`. An inject starts from an empty
 * directory and writes the `context.v1` file, its window opening at `start`;
 * a heal keeps what its inject left, the context it was given above all, and
 * writes a context only where the inject left none. Why it could not, as a
 * failure.
 */
const prepare = (instance: Instance, step: "inject" | "heal", start: { readonly lane_ms: number; readonly wall_ms: number }) =>
  Effect.try({
    try: () => {
      const layout = layoutOf(instance)
      if (step === "inject") fs.rmSync(instance.dir, { recursive: true, force: true })
      fs.mkdirSync(layout.state, { recursive: true })
      fs.mkdirSync(layout.out, { recursive: true })
      if (step === "heal" && fs.existsSync(layout.context)) return
      const context: Context.Context = {
        schema: Context.SCHEMA,
        scope: instance.cell === undefined ? "job" : "cell",
        window: { start },
        cells: [],
        ...(instance.cell === undefined ? {} : { planned: [instance.cell] }),
        ...(instance.lane === undefined ? {} : { lane: Context.laneOf(instance.lane) }),
        params: { ...instance.operation.manifest.params, ...instance.params }
      }
      fs.writeFileSync(layout.context, `${JSON.stringify(context, null, 2)}\n`)
    },
    catch: (cause) => `its directory ${instance.dir} not laid out: ${cause instanceof Error ? cause.message : String(cause)}`
  })

const now = Effect.map(Clock.currentTimeMillis, (clock_ms) => ({ clock_ms, wall_ms: Date.now() }))

/** {@link prepare} with the window opening now. */
const prepared = (instance: Instance, step: "inject" | "heal") =>
  Effect.flatMap(now, (at) => prepare(instance, step, { lane_ms: at.clock_ms, wall_ms: at.wall_ms }))

const emit = (sink: Sink, instance: Instance, phase: Phase, message?: string) =>
  Effect.flatMap(now, (at) => sink({ operation: instance.label, phase, ...at, ...(message === undefined ? {} : { message }) }))

/** Runs `step`'s executable of `instance` within `timeoutMs`, its output appended to `<step>.log`. */
const step = (instance: Instance, file: string, name: "inject" | "heal" | "probe", timeoutMs: number) => {
  const layout = layoutOf(instance)
  const log = path.join(instance.dir, `${name}.log`)
  return Effect.map(
    Executable.run({ dir: instance.operation.dir, file, ...layout, step: name, timeoutMs, log }),
    (ended) => ({ ended, said: `${Executable.describe(ended)}, its log ${log}` })
  )
}

/** Injects `instance`: its directory emptied and its context written, then `inject` run within its bound. */
export const inject = (instance: Instance, sink: Sink): Effect.Effect<void, InjectFailed> =>
  Effect.gen(function*() {
    const bounds = Operation.boundsOf(instance.operation.manifest)
    yield* emit(sink, instance, "inject")
    const laid = yield* Effect.result(prepared(instance, "inject"))
    if (Result.isFailure(laid)) {
      yield* emit(sink, instance, "failed", `inject not run: ${laid.failure}`)
      return yield* new InjectFailed({ operation: instance.label, reason: `not run: ${laid.failure}` })
    }
    const { ended, said } = yield* step(instance, instance.operation.manifest.run.inject, "inject", bounds.inject_s * 1000).pipe(
      Effect.onInterrupt(() => emit(sink, instance, "failed", "inject interrupted, its process group killed"))
    )
    if (!Executable.succeeded(ended)) {
      yield* emit(sink, instance, "failed", `inject ${said}`)
      return yield* new InjectFailed({ operation: instance.label, reason: said })
    }
    yield* emit(sink, instance, "injected")
  })

/** The probe of `instance` called until it answers other than "not yet" or its bound runs out; why not recovered, or `undefined`. */
const probe = (instance: Instance, file: string, bounds: Operation.Bounds) =>
  Effect.gen(function*() {
    const deadline = (yield* Clock.currentTimeMillis) + bounds.probe_s * 1000
    for (;;) {
      const left = deadline - (yield* Clock.currentTimeMillis)
      const { ended, said } = yield* step(instance, file, "probe", Math.max(1, left))
      if (Executable.succeeded(ended)) return undefined
      if (!(ended._tag === "Exited" && ended.code === Operation.NOT_YET)) return `probe ${said}`
      if ((yield* Clock.currentTimeMillis) + bounds.probe_interval_s * 1000 >= deadline) {
        return `probe still answers not yet after ${bounds.probe_s} s, its log ${path.join(instance.dir, "probe.log")}`
      }
      yield* Effect.sleep(bounds.probe_interval_s * 1000)
    }
  })

/** One heal attempt of `instance`: `heal`, then `settle`, then its probe; why it failed, or `undefined`. */
const attempt = (instance: Instance, sink: Sink, settle: Effect.Effect<void, string>) =>
  Effect.gen(function*() {
    const manifest = instance.operation.manifest
    const bounds = Operation.boundsOf(manifest)
    yield* emit(sink, instance, "heal")
    const laid = yield* Effect.result(prepared(instance, "heal"))
    if (Result.isFailure(laid)) {
      yield* emit(sink, instance, "failed", `heal not run: ${laid.failure}`)
      return `heal not run: ${laid.failure}`
    }
    const { ended, said } = yield* step(instance, manifest.run.heal, "heal", bounds.heal_s * 1000)
    if (!Executable.succeeded(ended)) {
      yield* emit(sink, instance, "failed", `heal ${said}`)
      return `heal ${said}`
    }
    const settled = yield* Effect.result(settle)
    if (Result.isFailure(settled)) {
      yield* emit(sink, instance, "failed", `heal: ${settled.failure}`)
      return `heal: ${settled.failure}`
    }
    yield* emit(sink, instance, "healed")
    const unproved = manifest.run.probe === undefined ? undefined : yield* probe(instance, manifest.run.probe, bounds)
    if (unproved !== undefined) {
      yield* emit(sink, instance, "failed", unproved)
      return unproved
    }
    yield* emit(sink, instance, "recovered")
    return undefined
  })

/** Why an attempt that ended `ended` failed, or `undefined`; one that died failed with its defect. */
const whyOf = (ended: Exit.Exit<string | undefined>): string | undefined =>
  Exit.isSuccess(ended) ? ended.value : `heal attempt died: ${Cause.pretty(ended.cause).split("\n")[0]}`

/**
 * Heals `instance`, uninterruptibly: up to {@link HEAL_ATTEMPTS} attempts,
 * the last failed one followed by `lab-left-changed`. An attempt that dies is
 * a failed one. `settle`, run in each attempt once `heal` succeeded and
 * before the probe, restores what the lane's orchestrator owns (a step's
 * system stood again); its failure fails the attempt. Whether it recovered.
 */
export const heal = (instance: Instance, sink: Sink, settle: Effect.Effect<void, string> = Effect.void): Effect.Effect<boolean> =>
  Effect.uninterruptible(Effect.gen(function*() {
    let why: string | undefined
    for (let tried = 1; tried <= HEAL_ATTEMPTS; tried++) {
      why = whyOf(yield* Effect.exit(attempt(instance, sink, settle)))
      if (why === undefined) return true
    }
    yield* emit(sink, instance, LAB_LEFT_CHANGED, `${HEAL_ATTEMPTS} heal attempts failed, the last: ${why}`)
    return false
  }))
