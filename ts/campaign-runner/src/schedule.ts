/**
 * The lane's cell scheduler: every cell runs once, at most `concurrency` at a
 * time, started no closer together than `startIntervalMs`, and two cells
 * holding one named lock never run at once. A cell takes its locks in name
 * order before its concurrency slot, so a cell waiting on a lock holds no slot
 * and two cells never wait on each other; it keeps them past its run while
 * what it came to holds them. A cell stating `alone` runs after every other,
 * one at a time, the lane held while what it came to holds it, then for its
 * heal time, so what it disturbed recovers before anything follows. Results
 * come back in the cells' order.
 */
import * as Clock from "effect/Clock"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Semaphore from "effect/Semaphore"

/** A cell that runs with nothing else on its lane. */
export interface Alone {
  /** The cell, in the run's log. */
  readonly name: string
  /** How long the lane holds once it ends, in ms of the scheduler's clock; 0 for none. */
  readonly healMs: number
}

export interface Locked {
  /** The named locks the cell holds while it runs. */
  readonly locks: ReadonlyArray<string>
  /** Present where the cell runs alone. */
  readonly alone?: Alone
}

export interface Pacing {
  readonly concurrency: number
  /** The least time between two starts; none when unset. */
  readonly startIntervalMs?: number
}

/**
 * Runs `cells` under `pacing`; a cell's named locks, and the lane of a cell
 * running alone, are kept, once its run gave its slot back, until `heldBy` of
 * what it came to resolves (its collectors that hold its locks answered and
 * closed, `@sip/rig` `CellCollectors`).
 */
export const schedule = <C extends Locked, A, E, R>(
  cells: ReadonlyArray<C>,
  pacing: Pacing,
  run: (cell: C) => Effect.Effect<A, E, R>,
  heldBy: (ran: A) => Effect.Effect<void> = () => Effect.void
): Effect.Effect<ReadonlyArray<A>, E, R> =>
  Effect.gen(function*() {
    const slots = yield* Semaphore.make(Math.max(1, pacing.concurrency))
    const shared = cells.filter((cell) => cell.alone === undefined)
    const names = [...new Set(shared.flatMap((cell) => cell.locks))].sort()
    const locks = new Map<string, Semaphore.Semaphore>()
    for (const name of names) locks.set(name, yield* Semaphore.make(1))
    const starts = yield* Semaphore.make(1)
    let nextStart = 0
    const paced = Effect.gen(function*() {
      if (pacing.startIntervalMs === undefined) return
      const now = yield* Clock.currentTimeMillis
      const at = Math.max(now, nextStart)
      nextStart = at + pacing.startIntervalMs
      if (at > now) yield* Effect.sleep(at - now)
    })
    const started = (cell: C) => Semaphore.withPermits(starts, 1)(paced).pipe(Effect.andThen(run(cell)))
    const one = (cell: C) => {
      const slotted = Effect.tap(Semaphore.withPermits(slots, 1)(started(cell)), heldBy)
      return [...new Set(cell.locks)].sort().reduceRight(
        (inner, name) => Semaphore.withPermits(locks.get(name)!, 1)(inner),
        slotted
      )
    }
    const ended = new Map<C, A>()
    const held = yield* Effect.forEach(shared, one, { concurrency: "unbounded" })
    shared.forEach((cell, i) => ended.set(cell, held[i]!))
    for (const cell of cells) {
      if (cell.alone === undefined) continue
      const exit = yield* Effect.exit(Effect.tap(started(cell), heldBy))
      if (cell.alone.healMs > 0) {
        yield* Console.log(`[heal] the lane holds ${cell.alone.healMs} ms after ${cell.alone.name}`)
        yield* Effect.sleep(cell.alone.healMs)
      }
      if (Exit.isFailure(exit)) return yield* Effect.failCause(exit.cause)
      ended.set(cell, exit.value)
    }
    return cells.map((cell) => ended.get(cell)!)
  })
