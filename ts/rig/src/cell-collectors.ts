/**
 * The cell-scope collectors of a batch (ADR-0060 item 5, appendix §3, §5).
 * Each cell is a scope of its own, in a directory of its own under the
 * plan's root (`CollectorPlan.cellPlanOf`): its collectors open before the
 * cell's call, then, once the call ended, collect, each `delay_s` (the lane's
 * override first) after the cell ended on the lane's clock, and close, while
 * the lane goes on to its next cells. A line naming no cell is the cell's.
 *
 * Each collector has a deadline of its own, its `collect_s` from when it was
 * due or, later, from when the one collecting before it answered: past it, it
 * answers `not checkable` on the cell, saying so, and the next one collects
 * (`CollectorRun.Options.due`). A judge of theirs, run once they all
 * answered, is bounded by its own `judge_s` (`judge-run.ts`): a cell's collect
 * ends by its last collector's deadline plus its judges' `judge_s`.
 *
 * A cell one of whose collectors `holds_cell_locks` keeps its own named locks
 * until its collectors answered or reached their deadlines, and closed
 * ({@link Ran.held}). The batch joins every cell's collectors before it reads
 * its end ({@link CellCollectors.joined}), for a bounded time where its caller
 * asks it ({@link CellCollectors.joinedWithin}); a batch that ends otherwise
 * interrupts them, and their `close` still runs.
 */
import * as Clock from "effect/Clock"
import * as Deferred from "effect/Deferred"
import * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
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
   * answered or reached their deadline, and closed.
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
  /**
   * {@link joined}, waiting `ms` at most: the cells whose collectors had not
   * answered by then have them interrupted and closed, each answering `not
   * checkable` on its cell, saying `why`.
   */
  readonly joinedWithin: (ms: number, why: string) => Effect.Effect<ReadonlyArray<CollectorRun.Collected>>
  /**
   * What the cells' collectors that ended by now came to, by cell in the order
   * the cells ran: their answers, or, where the batch's end interrupted them,
   * `not checkable` on their cell. Read once a batch that failed ended, it
   * answers for every cell that forked its collectors.
   */
  readonly settled: () => ReadonlyArray<CollectorRun.Collected>
  /** The labels of the instances whose `close` left the lab changed so far. */
  readonly leftChanged: () => ReadonlyArray<string>
}

/** A batch with no cell-scope collector: its cells run as they are. */
export const NONE: CellCollectors = {
  around: (_cell, run) => Effect.map(run, (value) => ({ value, held: Effect.void })),
  joined: Effect.succeed([]),
  joinedWithin: () => Effect.succeed([]),
  settled: () => [],
  leftChanged: () => []
}

/** A cell's collectors at work, forked: the cell, its instances, and the fork answering for them. */
interface Forked {
  readonly cell: string
  readonly instances: ReadonlyArray<CollectorRun.Instance>
  readonly fiber: Fiber.Fiber<ReadonlyArray<CollectorRun.Collected>>
}

/** What `forked` came to, `exit` its fork's end: its answers, or, interrupted, `not checkable` on its cell saying `why`. */
const outcomeOf = (forked: Forked, exit: Exit.Exit<ReadonlyArray<CollectorRun.Collected>>, why: string): ReadonlyArray<CollectorRun.Collected> =>
  Exit.isSuccess(exit) ? exit.value : notCheckableOf(forked.instances, forked.cell, why)

/** How long after its cell ended `instance` collects, in milliseconds: its lane's `delay_s`, else its manifest's, else none. */
export const delayMsOf = (instance: CollectorRun.Instance): number => {
  const overridden = instance.params[DELAY_KEY]
  return 1000 * (typeof overridden === "number" ? overridden : instance.collector.manifest.delay_s ?? 0)
}

/** The directory of the `n`-th cell's scope: file-safe, unique among the batch's. */
const dirNameOf = (n: number, cell: string): string => `${n}-${cell.replace(/[^A-Za-z0-9._-]/g, "_")}`

const NO_ANSWERS: CollectorRun.Answers = { verifications: [], attachments: [] }

/** What a cell's collectors answer that the batch interrupted before they answered. */
const INTERRUPTED = "interrupted before they answered: the batch ended"

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
    const forks: Array<Forked> = []
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
        const collect = Effect.flatMap(describe(value), (over) => (over === undefined ? Effect.succeed([]) : opened.collect(over)))
        const work = collect.pipe(
          Effect.map((got) => heldByCell(cell, got)),
          Effect.catchDefect((defect) => Effect.succeed(notCheckableOf(instances, cell, `its collectors died: ${errorText(defect)}`))),
          Effect.ensuring(Effect.andThen(Scope.close(scope, Exit.void), Effect.sync(() => void left.push(...opened.leftChanged())))),
          Effect.ensuring(Deferred.succeed(answered, undefined))
        )
        forks.push({ cell, instances, fiber: yield* Effect.forkIn(work, batch) })
        const holds = instances.some((instance) => instance.collector.manifest.holds_cell_locks === true)
        return { value, held: holds ? Deferred.await(answered) : Effect.void }
      })

    const joined = Effect.suspend(() =>
      Effect.map(Effect.forEach([...forks], (forked) => Effect.map(Fiber.await(forked.fiber), (exit) => outcomeOf(forked, exit, INTERRUPTED))), (all) => all.flat())
    )

    const joinedWithin = (ms: number, why: string) =>
      Effect.gen(function*() {
        const all = [...forks]
        yield* Effect.timeoutOption(Fiber.awaitAll(all.map((forked) => forked.fiber)), Duration.millis(Math.max(0, ms)))
        const running = all.filter((forked) => forked.fiber.pollUnsafe() === undefined)
        yield* Fiber.interruptAll(running.map((forked) => forked.fiber))
        const cut = running.filter((forked) => !Exit.isSuccess(forked.fiber.pollUnsafe()!))
        yield* Effect.forEach(cut.flatMap((forked) => forked.instances), (instance) => say(options.sink, instance.label, "failed", why), { discard: true })
        return all.flatMap((forked) => outcomeOf(forked, forked.fiber.pollUnsafe()!, cut.includes(forked) ? why : INTERRUPTED))
      })

    const settled = () =>
      forks.flatMap((forked) => {
        const exit = forked.fiber.pollUnsafe()
        return exit === undefined ? [] : outcomeOf(forked, exit, INTERRUPTED)
      })

    return { around, joined, joinedWithin, settled, leftChanged: () => [...left] } satisfies CellCollectors
  })
