/**
 * Planning a replay off the CUT STORE: one pinned prep, the captures it cut,
 * and the cells they become — and, per batch, the cells materialised.
 *
 * The prep is resolved ONCE per command and pinned for every capture the
 * command touches, as a CEILING: each capture is read as it stood at that prep,
 * which is the newest cut of it no later than the pin. That is what keeps a
 * re-cut landing mid-sweep from handing half a sweep one reading of a capture
 * and half another, and it costs nothing — the pin does not have to be the prep
 * that cut every capture, which no prep ever is once a targeted re-cut has run.
 * Because the ceiling is written on the run row, the reading each row answers is
 * reconstructible.
 *
 * A plan names cells; it materialises nothing. Cases are materialised BY BATCH
 * (`materialiseBatch`), one hash at a time, as the batch that runs them is
 * planned: a request of 40 cells materialises 40 cases, never the corpus, and
 * a request over a big capture materialises the cases it names and the flows
 * document once. What a cell then cites is a directory `Case::load`
 * cannot tell from a hand-authored one.
 *
 * Nothing here runs anything. A capture with no standing cut, a cut that broke
 * and a cut that refused every case are three different answers, and all
 * three come back as plans rather than as silence.
 */
import type { Campaign } from "@sip/contracts"
import { Cuts, type Model, Refusals, type Requests } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Content from "../content/content.js"
import type { Work } from "../universe.js"

/** One cell to run: a case of the pinned cut, and the key its capture names the call by. */
export interface CellRef {
  readonly capture: string
  readonly callKey: string
  readonly caseId: string
  /** The content hash the case materialises from; what the merged ledger is asked for. */
  readonly caseHash: string
  readonly egressEndpoint: string | null
}

/** One cell materialised: what the driver runs. */
export interface PlannedCell extends CellRef {
  readonly cell: Campaign.PivotReplayCell
}

/**
 * One capture of a replay: the run row it opens (its set kind) and what that
 * row lands. `cells` is the order the phase drives them in.
 */
export type CapturePlan =
  | {
    readonly _tag: "cells"
    readonly capture: string
    readonly kind: Model.SetKind
    readonly flowsHash: string | null
    readonly cells: ReadonlyArray<CellRef>
  }
  /** The cut refused every case, or the request's every key is gone: an answer, not a failure. */
  | { readonly _tag: "no-cases"; readonly capture: string; readonly kind: Model.SetKind; readonly detail: string }
  /** The store cannot plan it: an `error` row the queue shows. */
  | { readonly _tag: "broke"; readonly capture: string; readonly kind: Model.SetKind; readonly broke: string }

/**
 * The ceiling a command pins itself to: the prep it was told to read as of, or
 * the highest that has cut anything. `null` means the store holds no such prep,
 * which is a refusal rather than an empty sweep.
 */
export const pin = Effect.fn("Plan.pin")(function*(named?: number) {
  const cuts = yield* Cuts.Service
  if (named !== undefined) return (yield* cuts.exists(named)) ? named : null
  return yield* cuts.standingPrep
})

/**
 * `--case` ids the pinned cut holds no case of: refused before any row opens,
 * one line per id — the id, a token, and why.
 */
export class UnknownCases extends Data.TaggedError("ReplayDb.UnknownCases")<{
  readonly capture: string
  readonly absences: ReadonlyArray<Refusals.Absence>
  readonly prepId: number
}> {
  override get message(): string {
    return [
      `${this.absences.length} case(s) of ${this.capture} have no case in its cut as of prep${this.prepId}:`,
      ...this.absences.map((absence) => `  ${absence.caseId}  ${absence.token}: ${absence.reason}`)
    ].join("\n")
  }
}

const NO_CASES = "the cut refused every case"

const refOf = (capture: string, entry: Model.CutCase): CellRef => ({
  capture,
  callKey: entry.callKey,
  caseId: entry.caseId,
  caseHash: entry.caseHash,
  egressEndpoint: entry.egressEndpoint
})

/**
 * One capture's cells off its standing cut — the ids its set names where it is
 * a `cases` set, the whole cut where it is full. An id the cut holds nowhere
 * is the WHOLE invocation's refusal (`UnknownCases`): a run over the cases
 * that do exist would answer a question nobody asked. A cut that refused every
 * case holds none of them.
 */
const planWorkItem = Effect.fn("Plan.planWorkItem")(function*(prepId: number, work: Work) {
  const { capture } = work
  const kind = work.set.kind
  const cuts = yield* Cuts.Service
  const standing = yield* cuts.standing(capture, prepId)
  if (standing === null) {
    return {
      _tag: "broke",
      capture,
      kind,
      broke: `no cut of this capture as of prep${prepId} — cut it first`
    } satisfies CapturePlan
  }
  if (standing.broke !== null) return { _tag: "broke", capture, kind, broke: standing.broke } satisfies CapturePlan
  if (work.set.kind === "cases") {
    const held = new Set(standing.cases.map((entry) => entry.caseId))
    const unknown = work.set.cases.filter((id) => !held.has(id))
    if (unknown.length > 0) {
      const refusals = yield* Refusals.Service
      const absences = yield* Effect.forEach(unknown, (id) => refusals.whyNot(capture, id, prepId))
      return yield* Effect.fail(new UnknownCases({ capture, absences, prepId }))
    }
  }
  if (standing.cases.length === 0) return { _tag: "no-cases", capture, kind, detail: NO_CASES } satisfies CapturePlan
  const wanted = work.set.kind === "full" ? null : new Set(work.set.cases)
  const named = wanted === null ? standing.cases : standing.cases.filter((entry) => wanted.has(entry.caseId))
  return {
    _tag: "cells",
    capture,
    kind,
    flowsHash: standing.flowsHash,
    cells: named.map((entry) => refOf(capture, entry))
  } satisfies CapturePlan
})

/**
 * A work list — captures with their case sets, as the sweeps and `--case`
 * name them — planned off the cut store, in the order asked for. A cases set
 * naming an id the cut holds nowhere refuses the whole list, before anything
 * opens; a broken plan is that capture's own answer.
 */
export const planWork = Effect.fn("Plan.planWork")(function*(
  captures: ReadonlyArray<Work>,
  prepId: number,
  concurrency: number
) {
  return yield* Effect.forEach(captures, (work) => planWorkItem(prepId, work), {
    concurrency: Math.max(1, concurrency)
  })
})

/**
 * A resolved request as plans: one per capture, its kind as the
 * resolution derived it, its planned cells in key order. A capture whose every
 * key is gone opens a `no-cases` row saying so. The flows document's hash is
 * read off the capture's cut at the pinned prep, one query per capture.
 */
export const planRequest = Effect.fn("Plan.planRequest")(function*(resolution: Requests.Resolution) {
  const cuts = yield* Cuts.Service
  const plans: Array<CapturePlan> = []
  for (const entry of resolution.captures) {
    const { capture, kind } = entry
    if (entry.planned.length === 0) {
      // A whole capture whose cut holds no case is the cut's own answer, a
      // request whose every key is gone says so.
      const refused = entry.gone.filter((cell) => cell.gone === "refused").length
      plans.push({
        _tag: "no-cases",
        capture,
        kind,
        detail: entry.gone.length === 0
          ? NO_CASES
          : `every cell of the request is gone (${refused} refused, ${entry.gone.length - refused} not in this cut)`
      })
      continue
    }
    const standing = yield* cuts.standing(capture, resolution.prepId)
    plans.push({
      _tag: "cells",
      capture,
      kind,
      flowsHash: standing?.flowsHash ?? null,
      cells: entry.planned.map((cell) => refOf(capture, cell))
    })
  }
  return plans as ReadonlyArray<CapturePlan>
})

/** Every cell of a plan list, flat, in the order the phase drives them: capture, then cell. */
export const cellsOf = (plans: ReadonlyArray<CapturePlan>): ReadonlyArray<CellRef> =>
  plans.flatMap((entry) => (entry._tag === "cells" ? entry.cells : []))

export interface MaterialiseRequest {
  readonly cells: ReadonlyArray<CellRef>
  readonly lane: string
  /** Where `_content/` lives; the cache is a sibling of the sessions it serves. */
  readonly sessions: string
  /** Each capture's flows document, by hash; a capture with none cites no document. */
  readonly flowsHash: (capture: string) => string | null
  /** How many cells are materialised at once. */
  readonly concurrency: number
}

/** What one batch's materialisation came to: the cells that can run, and the captures that cannot. */
export interface Materialised {
  readonly cells: ReadonlyArray<PlannedCell>
  /** Why a capture's cells cannot run, by capture: its row closes `error` on this. */
  readonly broke: ReadonlyMap<string, string>
}

const firstLine = (text: string): string => text.split("\n")[0] ?? text

/**
 * One batch's cells on disk — each capture's flows document once, then each
 * case — in the order given. Whatever a cell's materialisation throws is its
 * CAPTURE's `broke`: a case the store cannot rebuild must not take the
 * captures beside it down, and the capture's other cells are not run against
 * a reading half of which is missing.
 */
export const materialiseBatch = Effect.fn("Plan.materialiseBatch")(function*(request: MaterialiseRequest) {
  const broke = new Map<string, string>()
  const flows = new Map<string, string | undefined>()
  const captures = [...new Set(request.cells.map((cell) => cell.capture))]
  yield* Effect.forEach(
    captures,
    (capture) => {
      const hash = request.flowsHash(capture)
      const file = Effect.gen(function*() {
        if (hash === null) return undefined
        return yield* Content.materialiseFlows(request.sessions, hash)
      })
      return file.pipe(
        Effect.map((materialised) => void flows.set(capture, materialised)),
        Effect.catchCause((cause) =>
          Effect.sync(() =>
            void broke.set(capture, `materialising the flows document failed: ${firstLine(Cause.pretty(cause))}`)
          )
        )
      )
    },
    { concurrency: Math.max(1, request.concurrency) }
  )
  const materialised = yield* Effect.forEach(
    request.cells,
    (ref) =>
      broke.has(ref.capture)
        ? Effect.succeed(undefined)
        : Content.materialise(request.sessions, ref.caseHash).pipe(
          Effect.map((caseDir): PlannedCell => {
            const file = flows.get(ref.capture)
            return {
              ...ref,
              cell: {
                kind: "pivot-replay",
                case: caseDir,
                lane: request.lane,
                ...(file === undefined ? {} : { flows: file }),
                ...(ref.egressEndpoint === null ? {} : { egressEndpoint: ref.egressEndpoint })
              }
            }
          }),
          Effect.catchCause((cause) =>
            Effect.sync(() => {
              broke.set(ref.capture, `materialising the cases failed: ${firstLine(Cause.pretty(cause))}`)
              return undefined
            })
          )
        ),
    { concurrency: Math.max(1, request.concurrency) }
  )
  return {
    cells: materialised.flatMap((cell) => (cell === undefined || broke.has(cell.capture) ? [] : [cell])),
    broke
  } satisfies Materialised
})
