/**
 * A corpus re-cut, SLICED: one prep, worked in slices that report as they land.
 *
 * The slices share one prep, because they are one cut. That makes the run
 * resumable for free: `capture_cuts` says which captures this prep already cut,
 * so a re-run continues where it stopped rather than re-cutting the corpus. The
 * prep is finished only once every slice has been cut, so `--resume` on an
 * unfinished prep is what a killed run costs.
 */
import { Cuts } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Fingerprint from "../fingerprint.js"
import type { Options } from "../options.js"
import * as Provenance from "../provenance.js"
import { censusGate, phase } from "./cut.js"
import * as CutRows from "./cut-rows.js"

/** Captures per slice: how much work a killed run loses, and how often it reports. */
export const DEFAULT_SLICE = 200

export interface SliceRequest {
  readonly captures: ReadonlyArray<string>
  readonly options: Options
  readonly size: number
  /** The prep to CONTINUE; a fresh one is opened where none is named. */
  readonly resume?: number
  /** The cut job this run is, where it is one: the prep names it. */
  readonly job?: number
  /** Told the slice about to be cut, 1-based, and how many there are: a job's row reads it as its batch. */
  readonly onSlice?: (at: number, of: number) => Effect.Effect<void>
  /** The cut phase itself, named so a test can drive the loop without a corpus. */
  readonly cut?: typeof phase
  /** The census gate, named so a test can drive the loop without a census run. */
  readonly gate?: typeof censusGate
}

/** What a sliced cut did. */
export interface Sliced {
  readonly prep: number
  /** Captures this run cut; the ones an earlier run already cut are not counted. */
  readonly cut: number
  /** Captures skipped because this prep had already cut them. */
  readonly resumed: number
  readonly broke: number
  readonly mirrored: CutRows.Mirrored
}

const sliceOf = <A>(items: ReadonlyArray<A>, size: number): ReadonlyArray<ReadonlyArray<A>> => {
  const slices: Array<ReadonlyArray<A>> = []
  for (let at = 0; at < items.length; at += size) slices.push(items.slice(at, at + size))
  return slices
}

/** Every slice cut in order, under the one prep they share. */
export const sliced = Effect.fn("Cut.sliced")(function*(request: SliceRequest) {
  const cuts = yield* Cuts.Service
  const run = request.cut ?? phase
  // The gate of the whole sliced cut, once, before its prep opens.
  yield* (request.gate ?? censusGate)(request.captures, request.options)
  const prep = request.resume ?? (yield* cuts.open(yield* Provenance.current, yield* Fingerprint.cut, request.job))
  const done = new Set(request.resume === undefined ? [] : yield* cuts.captures(prep))
  const slices = sliceOf(request.captures, Math.max(1, request.size))
  yield* Console.log(
    `[cut] prep${prep}: ${request.captures.length} capture(s) in ${slices.length} slice(s) ` +
      `of ${request.size}${done.size === 0 ? "" : `, ${done.size} already cut`}`
  )
  let outcome: Sliced = { prep, cut: 0, resumed: 0, broke: 0, mirrored: CutRows.NOTHING_MIRRORED }
  for (const [at, slice] of slices.entries()) {
    if (request.onSlice !== undefined) yield* request.onSlice(at + 1, slices.length)
    const todo = slice.filter((capture) => !done.has(capture))
    outcome = { ...outcome, resumed: outcome.resumed + (slice.length - todo.length) }
    if (todo.length === 0) {
      yield* Console.log(`[cut] prep${prep} slice ${at + 1}/${slices.length}: already cut`)
      continue
    }
    const cut = yield* run(todo, request.options, prep, request.job)
    const broke = [...cut.readings.values()].filter((r) => r.broke !== undefined).length
    outcome = {
      ...outcome,
      cut: outcome.cut + cut.readings.size,
      broke: outcome.broke + broke,
      mirrored: CutRows.plus(outcome.mirrored, cut.mirrored)
    }
    yield* Console.log(
      `[cut] prep${prep} slice ${at + 1}/${slices.length}: ${cut.readings.size} cut, ` +
        `${broke} broken — ${outcome.cut + outcome.resumed}/${request.captures.length} done`
    )
  }
  yield* cuts.finish(prep)
  return outcome
})
