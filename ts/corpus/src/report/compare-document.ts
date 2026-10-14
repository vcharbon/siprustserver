/**
 * The documents a selector names, built once for every reader: the compare
 * report of two run sets (`sip.compare/1`) and the run report of one
 * (`sip.run-report/1`). The compare command prints the first and a viewer
 * serves both, so the command and the page never disagree.
 *
 * A side whose selector pins no lane (no `@lane:`, run, sweep or job) is read
 * on the lane the caller names, so the current population never mixes two
 * lanes' cells of one call. A selector the store cannot answer, or that names
 * no run, is REFUSED with the reason. A cited `@run:N` whose row the wipe
 * dropped is reported off the summary its citation kept.
 */
import type { CompareReport, RunReport } from "@sip/contracts"
import { CaseDocuments, Compare, Cuts, RunSets, RunSummaries, Selector, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import * as CallLadders from "./call-ladders.js"
import * as Report from "./compare-report.js"
import * as Runs from "./run-report.js"

/** What a request came to: the document, or why the store cannot answer it. */
export type Answer<A> =
  | { readonly _tag: "document"; readonly document: A }
  | { readonly _tag: "refused"; readonly reason: string }

const refused = (reason: string): { readonly _tag: "refused"; readonly reason: string } => ({ _tag: "refused", reason })

/** The compare report, with the comparison it was made of: what `--request` freezes. */
export type CompareAnswer =
  | {
    readonly _tag: "document"
    readonly document: CompareReport.CompareReport
    readonly comparison: Compare.Comparison
  }
  | { readonly _tag: "refused"; readonly reason: string }

/** A side's text as a selector on `lane`, a bare integer read as a sweep where `sweeps` says so. */
export const sideOf = (
  text: string,
  lane: string,
  sweeps = false
): Effect.Effect<Selector.Selector | { readonly refused: string }, never, StoreBinding.Service> =>
  Effect.map(
    StoreBinding.parse(sweeps && /^\d+$/.test(text.trim()) ? `@sweep:${text.trim()}` : text),
    (parsed) => Selector.isRefused(parsed) ? parsed : Selector.onLane(parsed, lane)
  )

export interface CompareInput {
  readonly before: string
  readonly after: string
  /** The lane a side that pins none is read on. */
  readonly lane: string
  /** Every compared call, the unchanged ones too. */
  readonly all: boolean
  /** Only the calls whose two ladders differ, whatever their class; false where unstated. */
  readonly ladderChanged?: boolean
  /** How many listed calls get their ladders read. */
  readonly ladders: number
  /** How many calls an only-before/only-after list names. */
  readonly only: number
  /** The viewer every link points at. */
  readonly viewer: string
  /** A bare integer names a sweep (the store command's spelling), else a run. */
  readonly sweeps?: boolean
}

/** The compare report of two selectors, or why it is refused. */
export const compare = Effect.fn("CompareDocument.compare")(function*(input: CompareInput) {
  const before = yield* sideOf(input.before, input.lane, input.sweeps)
  const after = yield* sideOf(input.after, input.lane, input.sweeps)
  if (Selector.isRefused(before)) return refused(before.refused)
  if (Selector.isRefused(after)) return refused(after.refused)
  const sets = yield* RunSets.Service
  for (const side of [before, after]) {
    const why = yield* sets.refusal(side)
    if (why !== null) return refused(why)
  }
  const only = Math.max(0, input.only)
  const pair = yield* sets.pair(before, after, only)
  for (const side of [pair.before, pair.after]) {
    if (side.runs === 0) return refused(`${side.selector} names no run`)
  }
  const comparison = Compare.compare(pair.before, pair.after)
  const document = yield* Report.document(comparison, {
    all: input.all,
    ladderChanged: input.ladderChanged === true,
    ladders: Math.max(0, input.ladders),
    only,
    viewer: input.viewer
  }).pipe(Effect.provide(CallLadders.layer))
  return { _tag: "document", document, comparison } satisfies CompareAnswer
})

export interface ReportInput {
  readonly selector: string
  /** The lane a selector that pins none is read on. */
  readonly lane: string
  /** The viewer every link points at. */
  readonly viewer: string
  /** How many of the set's runs the report lists, newest first. */
  readonly runs: number
}

/** The one run a selector names by id alone, where it names one so. */
const onlyRun = (selector: Selector.Selector): number | undefined => {
  const [term, ...rest] = selector.terms
  return term !== undefined && term._tag === "run" && rest.every((one) => one._tag === "lane") ? term.id : undefined
}

/** The run report of a selector, or why it is refused. */
export const report = Effect.fn("CompareDocument.report")(function*(input: ReportInput) {
  const selector = yield* sideOf(input.selector, input.lane)
  if (Selector.isRefused(selector)) return refused(selector.refused)
  const sets = yield* RunSets.Service
  const why = yield* sets.refusal(selector)
  if (why !== null) return refused(why)
  const pair = yield* sets.pair(selector, selector, 0)
  if (pair.before.runs === 0) {
    const id = onlyRun(selector)
    const kept = id === undefined ? null : yield* (yield* RunSummaries.Service).of(id)
    if (kept === null) return refused(`${selector.text} names no run`)
    return {
      _tag: "document",
      document: Runs.fromSummary(selector.text, kept, input.viewer)
    } satisfies Answer<RunReport.RunReport>
  }
  const runs = yield* sets.runs(selector, Math.max(1, input.runs))
  return {
    _tag: "document",
    document: Runs.fromRuns({
      selector: selector.text,
      runs,
      total: pair.before.runs,
      side: pair.before,
      viewer: input.viewer
    })
  } satisfies Answer<RunReport.RunReport>
})

/** The services both documents read. */
export type Services =
  | RunSets.Service
  | RunSummaries.Service
  | CaseDocuments.Service
  | Cuts.Service
  | StoreBinding.Service
