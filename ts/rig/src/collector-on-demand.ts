/**
 * An on-demand run (ADR-0060 appendix §5): the collectors the site or the CLI
 * asks for (`CollectorPlan.onDemandOf`), over the cells it names, long after
 * their job ended. The instances are one scope of their own: their locks
 * taken once, each opened, collected over the cells and closed, whatever
 * ended the run (`CollectorRun.opened`). Their context's window is the one
 * the cells ran in, not the instant they are asked: a collector reading the
 * lab over its window reads what the calls left.
 */
import * as Effect from "effect/Effect"
import type * as Cell from "./cell.js"
import * as CollectorRun from "./collector-run.js"

/** What an on-demand run came to. */
export interface Ran {
  /** What each instance answered, in their order. */
  readonly collected: ReadonlyArray<CollectorRun.Collected>
  /** The labels of the instances whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
}

/** The window `cells` ran in, on the wall clock: the earliest start to the latest end; undefined for no cell. */
export const windowOf = (
  cells: ReadonlyArray<Cell.Cell>
): { readonly start: Cell.Instant; readonly end: Cell.Instant } | undefined => {
  if (cells.length === 0) return undefined
  const starts = cells.map((cell) => cell.timeline.start)
  const ends = cells.map((cell) => cell.timeline.end)
  return {
    start: starts.reduce((a, b) => (b.wall_ms < a.wall_ms ? b : a)),
    end: ends.reduce((a, b) => (b.wall_ms > a.wall_ms ? b : a))
  }
}

/** `instances` opened, collected over `over` and closed, as one scope whose window is the one `over`'s cells ran in. */
export const run = (
  instances: ReadonlyArray<CollectorRun.Instance>,
  over: CollectorRun.Over,
  options: CollectorRun.Options
): Effect.Effect<Ran> =>
  Effect.gen(function*() {
    let opened: CollectorRun.Opened | undefined
    const collected = yield* Effect.scoped(
      Effect.flatMap(CollectorRun.opened(instances, { ...options, window: windowOf(over.cells) }), (scope) => {
        opened = scope
        return scope.collect(over)
      })
    )
    return { collected, leftChanged: opened?.leftChanged() ?? [] }
  })
