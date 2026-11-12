/**
 * The cell-scope collectors of a batch (ADR-0060 item 5, appendix §3, §5).
 * Each cell is a scope of its own, in a directory of its own under the
 * plan's root (`CollectorPlan.cellPlanOf`): its collectors open before the
 * cell's call, then, once the call ended, collect, each `delay_s` (the lane's
 * override first) after the cell ended on the lane's clock, and close, while
 * the lane goes on to its next cells. A line naming no cell is the cell's.
 *
 * A cell's collectors have one deadline, the longest `delay_s` plus
 * `collect_s` among them after the cell ended: past it every one answers `not
 * checkable` on the cell, saying so, and the scope closes. A cell one of whose
 * collectors `holds_cell_locks` keeps its own named locks until its
 * collectors answered or reached that deadline ({@link Ran.held}). The batch
 * joins every cell's collectors before it reads its end
 * ({@link CellCollectors.joined}); a batch that ends otherwise interrupts
 * them, and their `close` still runs.
 */
import * as Clock from "effect/Clock"
import * as Deferred from "effect/Deferred"
import * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as Option from "effect/Option"
import * as Scope from "effect/Scope"
import * as path from "node:path"
import * as Collector from "./collector.js"
import type * as CollectorPlan from "./collector-plan.js"
import * as CollectorRun from "./collector-run.js"

/** The key of a collector's parameters under which a lane overrides its `delay_s`. */
export const DELAY_KEY = "delay_s"

/** A cell's call as it ended, and what its locks wait on. */
export interface Ran<A> {
  readonly value: A
  /**
   * Resolves once the cell's own named locks may be given back: at once, or,
   * where one of its collectors `holds_cell_locks`, once its collectors
   * answered or reached their deadline.
   */
  readonly held: Effect.Effect<void>
}

/** A batch's cell-scope collectors, as its driver runs its cells. */
export interface CellCollectors {
  /**
   * Runs `run`, the call of the cell `cell` (the id its description states),
   * its collectors opened before it; once it ended, forks their collect over
   * the cell as `describe` states it from what the call came to, and returns.
   * A cell `describe` states no description of collects nothing. A `run`
   * that fails closes the cell's collectors and fails.
   */
  readonly around: <A, E, R, R2>(
    cell: string,
    run: Effect.Effect<A, E, R>,
    describe: (value: A) => Effect.Effect<CollectorRun.Over | undefined, never, R2>
  ) => Effect.Effect<Ran<A>, E, R | R2>
  /** Waits for every cell's collectors to answer or reach their deadline: what they answered, by cell in the order the cells ran, each closed. */
  readonly joined: Effect.Effect<ReadonlyArray<CollectorRun.Collected>>
  /** The labels of the instances whose `close` left the lab changed so far. */
  readonly leftChanged: () => ReadonlyArray<string>
}

/** A batch with no cell-scope collector: its cells run as they are. */
export const NONE: CellCollectors = {
  around: (_cell, run) => Effect.map(run, (value) => ({ value, held: Effect.void })),
  joined: Effect.succeed([]),
  leftChanged: () => []
}

/** How long after its cell ended `instance` collects, in milliseconds: its lane's `delay_s`, else its manifest's, else none. */
export const delayMsOf = (instance: CollectorRun.Instance): number => {
  const overridden = instance.params[DELAY_KEY]
  return 1000 * (typeof overridden === "number" ? overridden : instance.collector.manifest.delay_s ?? 0)
}

/** The deadline of a cell's `instances` after the cell ended, in milliseconds: the longest `delay_s` plus `collect_s` among them. */
export const deadlineMsOf = (instances: ReadonlyArray<CollectorRun.Instance>): number =>
  Math.max(0, ...instances.map((instance) => delayMsOf(instance) + Collector.boundsOf(instance.collector.manifest).collect_s * 1000))

/** The directory of the `n`-th cell's scope: file-safe, unique among the batch's. */
const dirNameOf = (n: number, cell: string): string => `${n}-${cell.replace(/[^A-Za-z0-9._-]/g, "_")}`

const NO_ANSWERS: CollectorRun.Answers = { verifications: [], attachments: [] }

const errorText = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/** What `instances` answer of `cell` where they could not: `not checkable`, saying `message`, for each verification each names. */
const notCheckableOf = (instances: ReadonlyArray<CollectorRun.Instance>, cell: string, message: string): ReadonlyArray<CollectorRun.Collected> =>
  instances.map((instance) => ({
    label: instance.label,
    scope: NO_ANSWERS,
    cells: new Map([[cell, {
      verifications: Collector.answeredBy(instance.collector.manifest).map((name) => ({
        name,
        source: instance.collector.name,
        answer: "not-checkable" as const,
        message
      })),
      attachments: []
    }]]),
    keyed: [],
    assigned: [],
    keys: []
  }))

/**
 * `collected` held by `cell` alone: each line of the scope's own that the
 * cell does not hold of that name joined to the cell's. The scope's
 * attachments (a judge's unclaimed records) stay in its directory.
 */
const heldByCell = (cell: string, collected: ReadonlyArray<CollectorRun.Collected>): ReadonlyArray<CollectorRun.Collected> =>
  collected.map((one) => {
    const own = one.cells.get(cell) ?? NO_ANSWERS
    const names = new Set(own.verifications.map((line) => line.name))
    const verifications = [...own.verifications, ...one.scope.verifications.filter((line) => !names.has(line.name))]
    return { ...one, scope: NO_ANSWERS, cells: new Map([[cell, { verifications, attachments: own.attachments }]]) }
  })

/** The event `phase` of `collector`, saying `message`, told `sink` now. */
const say = (sink: CollectorRun.Sink, collector: string, phase: CollectorRun.Phase, message: string) =>
  Effect.flatMap(Clock.currentTimeMillis, (clock_ms) => sink({ collector, phase, clock_ms, wall_ms: Date.now(), message }))

/**
 * Tells `sink`, for each collector `plan` names unrun, that it runs on no cell
 * of the batch: a cell-scope collector does not run on a simulated lane. An
 * event `not-run` each, informative: it refuses nothing.
 */
export const notRun = (plan: CollectorPlan.CellPlan, sink: CollectorRun.Sink): Effect.Effect<void> =>
  Effect.forEach(
    plan.unrun,
    (name) => say(sink, name, "not-run", `a cell-scope collector runs on no simulated lane, and ${plan.lane} is one`),
    { discard: true }
  )

/** `delay` after `from` on the clock, the instant it is due; at once where it is past. */
const until = (from: number, delay: number) =>
  Effect.flatMap(Clock.currentTimeMillis, (now) => (from + delay > now ? Effect.sleep(from + delay - now) : Effect.void))

/**
 * The cell-scope collectors of `plan` for the batch at hand, run with
 * `options`: their forks in the caller's scope, the batch's, which interrupts
 * those still running when it closes.
 */
export const make = (plan: CollectorPlan.CellPlan, options: CollectorRun.Options): Effect.Effect<CellCollectors, never, Scope.Scope> =>
  Effect.gen(function*() {
    if (plan.runs.length === 0) return NONE
    const batch = yield* Effect.scope
    const forks: Array<Fiber.Fiber<ReadonlyArray<CollectorRun.Collected>>> = []
    const left: Array<string> = []
    let cells = 0

    const around = <A, E, R, R2>(
      cell: string,
      run: Effect.Effect<A, E, R>,
      describe: (value: A) => Effect.Effect<CollectorRun.Over | undefined, never, R2>
    ): Effect.Effect<Ran<A>, E, R | R2> =>
      Effect.gen(function*() {
        const instances = plan.instancesFor(cell, path.join(plan.root, dirNameOf(++cells, cell)))
        if (instances.length === 0) return { value: yield* run, held: Effect.void }
        const scope = yield* Scope.fork(batch, "sequential")
        let ended = 0
        const due = (instance: CollectorRun.Instance) => until(ended, delayMsOf(instance))
        const opened = yield* Scope.provide(scope)(CollectorRun.opened(instances, { ...options, due }))
        const value = yield* run.pipe(Effect.onExit((exit) => (Exit.isSuccess(exit) ? Effect.void : Scope.close(scope, exit))))
        ended = yield* Clock.currentTimeMillis
        const answered = yield* Deferred.make<void>()
        const deadline = deadlineMsOf(instances)
        const late = `not answered within its deadline of ${deadline / 1000} s after the cell ended`
        const collect = Effect.flatMap(describe(value), (over) => (over === undefined ? Effect.succeed([]) : opened.collect(over)))
        const work = collect.pipe(
          Effect.timeoutOption(Duration.millis(deadline)),
          Effect.flatMap((got) =>
            Option.isSome(got)
              ? Effect.succeed(heldByCell(cell, got.value))
              : Effect.as(Effect.forEach(instances, (instance) => say(options.sink, instance.label, "failed", late), { discard: true }), notCheckableOf(instances, cell, late))
          ),
          Effect.catchDefect((defect) => Effect.succeed(notCheckableOf(instances, cell, `its collectors died: ${errorText(defect)}`))),
          Effect.ensuring(Deferred.succeed(answered, undefined)),
          Effect.ensuring(Effect.andThen(Scope.close(scope, Exit.void), Effect.sync(() => void left.push(...opened.leftChanged()))))
        )
        forks.push(yield* Effect.forkIn(work, batch))
        const holds = instances.some((instance) => instance.collector.manifest.holds_cell_locks === true)
        return { value, held: holds ? Deferred.await(answered) : Effect.void }
      })

    return {
      around,
      joined: Effect.map(Effect.suspend(() => Effect.forEach([...forks], Fiber.join)), (all) => all.flat()),
      leftChanged: () => [...left]
    } satisfies CellCollectors
  })
