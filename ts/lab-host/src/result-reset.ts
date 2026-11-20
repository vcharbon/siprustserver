/**
 * A result reset: the store's (`@sip/store` `ResultReset`) over the result
 * tables a deployment names: every run, case result, verification line,
 * batch and campaign record emptied, nothing converted; the frozen ids, the
 * population, the cuts and the summaries of cited runs kept.
 *
 * The pins it drops are listed before anything is written. Without `confirm`
 * it plans in a read-only transaction; with it, the pack's freeze runs first
 * (every cited run summarised, so a citation outlives its run), then the
 * reset. The bundles on disk stay.
 */
import { ResultReset } from "@sip/store"
import * as Effect from "effect/Effect"
import { type KeepPack, scan } from "./store-keep.js"

export interface Request {
  readonly sessions: string
  /** Without it the reset plans, read-only, and empties nothing. */
  readonly confirm: boolean
  /** A citations file read instead of the tracker. */
  readonly citations?: string
}

/** The plan as the lines the command prints: the pins first, then what is emptied and what is kept. */
export const planLines = (plan: ResultReset.Plan): ReadonlyArray<string> => [
  plan.pins.length === 0
    ? "[reset] no pin is dropped"
    : `[reset] ${plan.pins.length} pin(s) dropped with their runs:`,
  ...plan.pins.map((pin) =>
    `[reset]   run ${pin.runId} ${pin.capture} on ${pin.lane}, pinned ${pin.pinnedAt.slice(0, 10)}` +
    (pin.ticket === null ? "" : ` for ${pin.ticket}`)
  ),
  `[reset] emptied: ${plan.dropped.runs} run(s), ${plan.dropped.caseResults} case result(s), ` +
  `${plan.dropped.verifications} verification line(s), ${plan.dropped.batches} batch(es)`,
  `[reset] tables emptied: ${plan.emptied.join(", ")}`,
  `[reset] kept: ${plan.kept.caseIds} frozen case id(s), ${plan.kept.population} capture(s) of the population, ` +
  `${plan.kept.captureCuts} capture cut(s)`,
  ...plan.holders.map((holder) => `[reset] refused while: ${holder}`)
]

/** The reset of the result tables `tables`: planned, or — confirmed — frozen by `pack`, then executed. */
export const reset = <E, R>(pack: Pick<KeepPack<E, R>, "freeze">, tables: ReadonlyArray<string>, request: Request) =>
  Effect.gen(function*() {
    const store = yield* ResultReset.Service
    const named = { tables }
    const plan = yield* store.plan(named)
    for (const line of planLines(plan)) console.log(line)
    if (!request.confirm) {
      console.log("[reset] dry run: nothing written; --confirm freezes, then empties the results")
      return plan
    }
    const { citations } = yield* scan(request.citations)
    yield* pack.freeze(citations, request.sessions)
    const done = yield* store.execute(named)
    console.log(`[reset] done: ${done.dropped.runs} run(s) and ${done.pins.length} pin(s) dropped`)
    return done
  }).pipe(Effect.withSpan("ResultReset.reset"))
