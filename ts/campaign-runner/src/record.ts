/**
 * The run's record in the run store: one campaign run row for the run (the
 * pack's {@link RunStore}), one store `runs` row per lane, both under the
 * campaign's name as their capture, opened before the first phase so a run
 * that ends early still says so on every lane, its bundle the directory every
 * cell of the lane writes under. Each cell is a case result (`case_id` the ref
 * and its sub-case, `case-result.ts`) holding its verification lines where it
 * ran, each failed check one of them over the pack's own checks, and the
 * attachments its bundle lists, with its cell and its log beside it (the
 * pack's log collector's last line of it, {@link logOf}), and every attempt
 * of a cell replayed more than once; each ref no case file covers is
 * a skipped `not-ported` result of every lane.
 *
 * A lane's run row is set ASIDE (`@sip/store` `aside.ts`): the campaign reads
 * it through its campaign run row, and no reading that chooses a run (the
 * triage queue, a capture's standing) ever lands on it. The environment that
 * made it is the campaign run row's. A run that ends before its report (a
 * crash, an interrupt) closes every lane still open as an error
 * ({@link abort}); one whose setup left the lab changed makes each lane an
 * error, closed before or after it was learnt ({@link leftChanged}).
 */
import { Aside, Runs } from "@sip/store"
import type { CellRecord } from "@sip/store/campaign-runs"
import type * as Model from "@sip/store/model"
import type { RunIdentity } from "@sip/store/run-identity"
import type * as Arr from "effect/Array"
import * as Effect from "effect/Effect"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import { attachedResultOf, attemptsOf, notPortedOf } from "./case-result.js"
import type { CellResult, Lane } from "./cell.js"
import type { CampaignPack } from "./pack.js"
import * as Results from "@sip/store/campaign-results"

export type { CellRecord }

/** The mode of a provisioned lane's run row. */
export const MODE = "provisioned"

/** What a campaign run row states when opened, beside the columns `X` the pack's store adds. */
export interface RunRequest {
  readonly capture: string
  readonly env: string | null
  readonly branch: string | null
  readonly seed: string | null
  readonly dir: string
  readonly scope: string | null
  readonly selection: Results.RunSelection
}

/** The pack's store of campaign run rows (over `@sip/store` `CampaignRuns`): what the record writes of a run beside its lanes' store rows. */
export interface RunStore<X extends object = object> {
  readonly open: (request: RunRequest & X) => Effect.Effect<number, SqlError>
  /** Ties a lane's store run row to the campaign run. */
  readonly link: (campaignRunId: number, runId: number) => Effect.Effect<void, SqlError>
  /** A case result's cell, for a result its run row already holds, replacing any written before. */
  readonly writeCell: (runId: number, caseId: string, cell: CellRecord) => Effect.Effect<void, SqlError>
  readonly finish: (campaignRunId: number) => Effect.Effect<void, SqlError>
}

/** One lane's open row. */
export interface LaneRow {
  readonly lane: Lane
  readonly runId: number
  /** Where the lane's cells write their bundles. */
  readonly bundlePath: string
}

export interface Opening<R, X extends object = object> {
  /** The campaign the run is of: the capture its rows are recorded under. */
  readonly capture: string
  readonly sessions: string
  readonly dir: string
  readonly seed: string | null
  /** What the run covers where it is not every case; null for a full run. */
  readonly scope: string | null
  /** What its tags selected. */
  readonly selection: Results.RunSelection
  /** The columns the pack's store adds to the campaign run row. */
  readonly columns: X
  readonly provenance: Model.Provenance
  /** The campaign job the run is, where it was launched as one. */
  readonly jobId?: number
  readonly identity: (lane: string) => Effect.Effect<RunIdentity, never, R>
  /** The catalog lane id a lane's run row names. */
  readonly idOf: (lane: Lane) => string
}

/** An open run's rows: its campaign run row, each lane's, and the lanes already closed with the error each was closed with. */
export interface Record {
  readonly id: number
  readonly capture: string
  readonly lanes: ReadonlyArray<LaneRow>
  /** Each closed lane's run row, and why it is an error; undefined where it ran to its end. */
  readonly closed: Map<number, string | undefined>
  /** Why a setup left the lab changed, once learnt ({@link leftChanged}): every lane closed afterwards carries it in its error. */
  readonly labLeftChanged: Array<string>
  /** The campaign job the run is, owner of its lane rows and its batches' rows; none for a run that is no job. */
  readonly jobId?: number
}

/** The campaign run row in `store` and every lane's run row; the campaign run row's environment and branch are its first lane's. */
export const open = <R, X extends object>(
  store: RunStore<X>,
  lanes: Arr.NonEmptyReadonlyArray<Lane>,
  opening: Opening<R, X>
) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const first = yield* opening.identity(opening.idOf(lanes[0]))
    const id = yield* store.open({
      ...opening.columns,
      capture: opening.capture,
      env: first.env,
      branch: first.branch,
      seed: opening.seed,
      dir: opening.dir,
      scope: opening.scope,
      selection: opening.selection
    })
    const rows: Array<LaneRow> = []
    for (const lane of lanes) {
      const opened = yield* runs.open({
        capture: opening.capture,
        lane: opening.idOf(lane),
        mode: MODE,
        sessionDir: path.join(opening.sessions, opening.capture, Aside.DIR),
        ...(opening.jobId === undefined ? {} : { jobId: opening.jobId }),
        provenance: opening.provenance,
        identity: { ...(yield* opening.identity(opening.idOf(lane))), env: Aside.ENV }
      })
      yield* store.link(id, opened.id)
      rows.push({ lane, runId: opened.id, bundlePath: opened.bundlePath })
    }
    return {
      id,
      capture: opening.capture,
      lanes: rows,
      closed: new Map<number, string | undefined>(),
      labLeftChanged: [],
      ...(opening.jobId === undefined ? {} : { jobId: opening.jobId })
    } satisfies Record
  })

/**
 * A cell's log as its row states it, off the last line of the log collector
 * `collector` the job's collectors answered of it: its records counted where
 * it passed, missing for its message where it was not checkable; missing for
 * `unlogged` where no line was answered and the run says why, else none.
 */
export const logOf = (collector: string, result: CellResult, unlogged: string | undefined): Results.Log | null => {
  const line = (result.collected ?? []).filter((one) => one.name === collector && one.source === collector).at(-1)
  if (line === undefined) return unlogged === undefined ? null : { missing: unlogged }
  const records = line.detail?.["records"]
  return line.answer === "pass" && typeof records === "number" ? { records } : { missing: line.message ?? line.answer }
}

export interface Closing {
  readonly record: Record
  readonly row: LaneRow
  readonly results: ReadonlyArray<CellResult>
  readonly notPorted: ReadonlyArray<string>
  /** Why the lane's run is an error; undefined where it ran to its end. */
  readonly error: string | undefined
  /** Why a cell the job's collectors answered no log of has none; its row states no log where undefined. */
  readonly unlogged: string | undefined
}

/** `error`, then why `record`'s setup left the lab changed where it did; undefined where neither is. */
const errorOf = (record: Record, error: string | undefined): string | undefined => {
  const said = [...(error === undefined ? [] : [error]), ...record.labLeftChanged]
  return said.length === 0 ? undefined : said.join("; ")
}

/**
 * A lane's row closed with its case results over the pack's own checks, each
 * cell's record beside its result in `store`; an error where the lab was left
 * changed.
 */
export const close = (store: Pick<RunStore, "writeCell">, pack: Pick<CampaignPack, "own" | "logCollector">, closing: Closing) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const cases = [
      ...yield* Effect.forEach(closing.results, (result) => attachedResultOf(pack.own, result)),
      ...closing.notPorted.map(notPortedOf)
    ]
    const error = errorOf(closing.record, closing.error)
    yield* runs.finish({
      id: closing.row.runId,
      status: error === undefined ? "done" : "error",
      ...(error === undefined ? {} : { detail: error }),
      cases
    })
    closing.record.closed.set(closing.row.runId, error)
    for (const result of closing.results) {
      if (result.bundle === undefined) continue
      const attempts = attemptsOf(result)
      yield* store.writeCell(closing.row.runId, Results.caseIdOf(result.cell.ref, result.cell.subcase), {
        cell: path.basename(result.bundle),
        log: logOf(pack.logCollector, result, closing.unlogged),
        ...(attempts === undefined ? {} : { attempts })
      })
    }
  })

/** Every lane `record` holds still open closed as an error for `reason` (and the lab left changed, where it was), and the run finished in `store`. */
export const abort = (store: Pick<RunStore, "finish">, record: Record, reason: string) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const detail = errorOf(record, reason) ?? reason
    for (const row of record.lanes) {
      if (record.closed.has(row.runId)) continue
      yield* runs.finish({ id: row.runId, status: "error", detail, cases: [] })
      record.closed.set(row.runId, detail)
    }
    yield* store.finish(record.id)
  })

/**
 * `reason` a setup operation left the lab changed, learnt once its heals ran:
 * every lane `record` closed made an error, its detail the error it was closed
 * with then `reason`, its case results kept; every lane closed afterwards, by
 * the report or {@link abort}, carries `reason` too.
 */
export const leftChanged = (record: Record, reason: string) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    record.labLeftChanged.push(reason)
    for (const [runId, error] of record.closed) {
      const detail = error === undefined ? reason : `${error}; ${reason}`
      yield* runs.finish({ id: runId, status: "error", detail, cases: [] })
      record.closed.set(runId, detail)
    }
  })
