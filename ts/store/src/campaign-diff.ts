/**
 * The DIFF of two campaign runs (`campaign-runs.ts`), lane by lane: the cells
 * whose status or verification lines moved, the cells only one run holds, and
 * the counts of what each run captured (the attachments its collectors
 * stored, by holder scope and name; the batch and job lines, by what they
 * answered; the backend log's records), ending in a verdict: `same` where no
 * cell and no count moved.
 *
 * {@link load} reads a side from the store, {@link diffOf} compares two sides
 * and {@link render} prints the comparison, its verdict last.
 */
import type { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import { type CampaignRun, type Result, statusOf } from "./campaign-results.js"
import * as CampaignRuns from "./campaign-runs.js"

/** One count of what a run captured: on `lane`, or for the run's job where `lane` is null. */
export interface Count {
  readonly lane: string | null
  /** What is counted: `cell attachment sut-log`, `batch line lab-guard/collector=pass`. */
  readonly what: string
  readonly count: number
}

/** One side of a diff: a campaign run and what it captured. */
export interface Side {
  readonly run: CampaignRun
  readonly counts: ReadonlyArray<Count>
}

/** The store holds no campaign run `id`. */
export class Missing extends Schema.TaggedError<Missing>()("CampaignDiff.Missing", {
  id: Schema.Number
}) {
  override get message(): string {
    return `the store holds no campaign run ${this.id}`
  }
}

interface CountRow {
  readonly lane: string | null
  readonly what: string
  readonly count: number
}

/**
 * The attachments and the batch and job lines of the campaign run `id`,
 * counted: a cell's and a batch's under the lane they ran on, the job's under
 * no lane. A job is counted once however many lanes it drove.
 */
const storedCounts = (id: number): Effect.Effect<ReadonlyArray<Count>, SqlError, SqlClient> =>
  Effect.flatMap(SqlClient, (sql) =>
    sql<CountRow>`
      WITH lanes AS (
        SELECT r.id AS run_id, r.lane, r.job_id
        FROM campaign_run_lanes l JOIN runs r ON r.id = l.run_id
        WHERE l.campaign_run_id = ${id}
      ), batch AS (
        SELECT b.id, b.lane FROM batches b
        WHERE EXISTS (SELECT 1 FROM lanes WHERE lanes.job_id = b.job_id AND lanes.lane = b.lane)
      ), job AS (
        SELECT DISTINCT job_id FROM lanes WHERE job_id IS NOT NULL
      )
      SELECT lanes.lane, 'cell attachment ' || a.name AS what, count(*)::int AS count
      FROM lanes JOIN attachments a ON a.scope = 'cell' AND a.run_id = lanes.run_id
      GROUP BY 1, 2
      UNION ALL
      SELECT batch.lane, 'batch attachment ' || a.name, count(*)::int
      FROM batch JOIN attachments a ON a.scope = 'batch' AND a.batch_id = batch.id
      GROUP BY 1, 2
      UNION ALL
      SELECT NULL, 'job attachment ' || a.name, count(*)::int
      FROM job JOIN job_attachments a ON a.job_id = job.job_id
      GROUP BY 2
      UNION ALL
      SELECT batch.lane, 'batch line ' || v.name || '/' || v.source || '=' || v.answer, count(*)::int
      FROM batch JOIN verifications v ON v.scope = 'batch' AND v.batch_id = batch.id
      GROUP BY 1, 2
      UNION ALL
      SELECT NULL, 'job line ' || v.name || '/' || v.source || '=' || v.answer, count(*)::int
      FROM job JOIN verifications v ON v.scope = 'job' AND v.job_id = job.job_id
      GROUP BY 2
      ORDER BY 1, 2
    `)

/** The backend log of `run`'s cells, counted per lane: the records kept, and the cells whose log is missing. */
const logCounts = (run: CampaignRun): ReadonlyArray<Count> =>
  run.lanes.flatMap((lane) => {
    const logs = lane.results.flatMap((result) => (result.log === null ? [] : [result.log]))
    const records = logs.reduce((sum, log) => sum + ("records" in log ? log.records : 0), 0)
    const missing = logs.filter((log) => "missing" in log).length
    return [
      ...(logs.length === 0 ? [] : [{ lane: lane.lane, what: "backend log records", count: records }]),
      ...(missing === 0 ? [] : [{ lane: lane.lane, what: "backend log missing", count: missing }])
    ]
  })

/** The campaign run `id` and what it captured; fails with {@link Missing} where the store holds none. */
export const load = Effect.fn("CampaignDiff.load")(function*(id: number) {
  const sql = yield* SqlClient
  const runs = yield* CampaignRuns.Service
  const [named] = yield* sql<{ readonly capture: string }>`SELECT capture FROM campaign_runs WHERE id = ${id}`
  if (named === undefined) return yield* new Missing({ id })
  const [run] = yield* runs.read(named.capture, { ids: [id] })
  if (run === undefined) return yield* new Missing({ id })
  return { run, counts: [...(yield* storedCounts(id)), ...logCounts(run)] } satisfies Side
})

/** A cell as a diff reads it: its status and its lines, each `name/source=answer`, sorted. */
export interface Cell {
  readonly status: Verification.CellStatus
  readonly lines: ReadonlyArray<string>
}

/** One line as a diff compares it: who answered what, the check a failing line states, and what took it out of the status. */
const lineOf = (line: Verification.Verification): string => {
  const anchor = line.detail?.["anchor"]
  const field = line.detail?.["field"]
  const check = typeof anchor === "string" && typeof field === "string" ? ` [${anchor} ${field}]` : ""
  const mark = line.waived === true ? " (waived)" : line.informative === true ? " (informative)" : ""
  return `${line.name}/${line.source}=${line.answer}${check}${mark}`
}

/** `result` as a diff reads it. */
export const cellOf = (result: Result): Cell => ({
  status: statusOf(result),
  lines: result.verifications.map(lineOf).sort()
})

/** A cell that differs between the two runs; a side that does not hold it is undefined. */
export interface Moved {
  readonly caseId: string
  readonly before: Cell | undefined
  readonly after: Cell | undefined
}

/** A count that differs between the two runs, or stands on both sides alike. */
export interface CountPair {
  readonly what: string
  readonly before: number
  readonly after: number
}

/** One lane of a diff; `job` for what the runs' jobs captured outside any lane. */
export interface LaneDiff {
  readonly lane: string
  /** The cells both runs hold on the lane. */
  readonly shared: number
  /** The cells whose status or lines differ, and those only one run holds, by case id. */
  readonly moved: ReadonlyArray<Moved>
  /** Every count of either run, by what it counts. */
  readonly counts: ReadonlyArray<CountPair>
}

export interface Diff {
  readonly before: CampaignRun
  readonly after: CampaignRun
  readonly lanes: ReadonlyArray<LaneDiff>
  readonly job: ReadonlyArray<CountPair>
}

const sameCell = (a: Cell, b: Cell): boolean =>
  a.status === b.status && a.lines.length === b.lines.length && a.lines.every((line, at) => line === b.lines[at])

const pairsOf = (before: ReadonlyArray<Count>, after: ReadonlyArray<Count>): ReadonlyArray<CountPair> => {
  const names = [...new Set([...before, ...after].map((count) => count.what))].sort()
  const of = (counts: ReadonlyArray<Count>, what: string): number =>
    counts.filter((count) => count.what === what).reduce((sum, count) => sum + count.count, 0)
  return names.map((what) => ({ what, before: of(before, what), after: of(after, what) }))
}

/** The diff of `after` against `before`, every lane either run drove, in name order. */
export const diffOf = (before: Side, after: Side): Diff => {
  const names = [...new Set([...before.run.lanes, ...after.run.lanes].map((lane) => lane.lane))].sort()
  const cellsOf = (side: Side, lane: string): ReadonlyMap<string, Cell> =>
    new Map(
      side.run.lanes.filter((one) => one.lane === lane).flatMap((one) => one.results).map((result) => [result.caseId, cellOf(result)])
    )
  const countsOf = (side: Side, lane: string | null) => side.counts.filter((count) => count.lane === lane)
  return {
    before: before.run,
    after: after.run,
    lanes: names.map((lane) => {
      const was = cellsOf(before, lane)
      const is = cellsOf(after, lane)
      const ids = [...new Set([...was.keys(), ...is.keys()])].sort()
      const moved = ids.flatMap((caseId): ReadonlyArray<Moved> => {
        const a = was.get(caseId)
        const b = is.get(caseId)
        return a !== undefined && b !== undefined && sameCell(a, b) ? [] : [{ caseId, before: a, after: b }]
      })
      return {
        lane,
        shared: ids.filter((caseId) => was.has(caseId) && is.has(caseId)).length,
        moved,
        counts: pairsOf(countsOf(before, lane), countsOf(after, lane))
      }
    }),
    job: pairsOf(countsOf(before, null), countsOf(after, null))
  }
}

const movedCount = (pairs: ReadonlyArray<CountPair>): number => pairs.filter((pair) => pair.before !== pair.after).length

/** Whether nothing differs: no cell moved, none stands on one side only, no count moved. */
export const isSame = (diff: Diff): boolean =>
  diff.lanes.every((lane) => lane.moved.length === 0 && movedCount(lane.counts) === 0) && movedCount(diff.job) === 0

const headOf = (run: CampaignRun): string =>
  `${run.id} (${run.capture}, env ${run.env ?? "-"}, ${run.branch ?? "-"}, ${run.startedAt}` +
  `${run.scope === null ? "" : `, scope ${run.scope}`}${run.finishedAt === null ? ", unfinished" : ""})`

const signed = (delta: number): string => (delta > 0 ? `+${delta}` : String(delta))

/** The lines of one group of counts: each one that moved, then the ones that stand alike on one line. */
const countLines = (pairs: ReadonlyArray<CountPair>): ReadonlyArray<string> => {
  const alike = pairs.filter((pair) => pair.before === pair.after)
  return [
    ...pairs.filter((pair) => pair.before !== pair.after).map((pair) =>
      `  captured  ${pair.what}: ${pair.before} → ${pair.after} (${signed(pair.after - pair.before)})`
    ),
    ...(alike.length === 0 ? [] : [`  captured alike  ${alike.map((pair) => `${pair.what} ${pair.before}`).join(", ")}`])
  ]
}

/** The lines of one moved cell: its statuses, then the lines only one side holds. */
const cellLines = (moved: Moved, before: number, after: number): ReadonlyArray<string> => {
  if (moved.before === undefined) return [`  only in ${after}  ${moved.caseId}: ${moved.after?.status ?? "-"}`]
  if (moved.after === undefined) return [`  only in ${before}  ${moved.caseId}: ${moved.before.status}`]
  const was = moved.before.lines
  const is = moved.after.lines
  const status = moved.before.status === moved.after.status
    ? `${moved.after.status}, lines moved`
    : `${moved.before.status} → ${moved.after.status}`
  return [
    `  moved  ${moved.caseId}: ${status}`,
    ...was.filter((line) => !is.includes(line)).map((line) => `      - ${line}`),
    ...is.filter((line) => !was.includes(line)).map((line) => `      + ${line}`)
  ]
}

/**
 * The diff as text: the two runs, then per lane its moved cells and its
 * counts, then the job's counts, and the verdict last: `DIFF same (…)` or
 * `DIFF moved (…)`.
 */
export const render = (diff: Diff): ReadonlyArray<string> => {
  const one = diff.lanes.flatMap((lane) => lane.moved).filter((moved) => moved.before === undefined || moved.after === undefined).length
  const moved = diff.lanes.reduce((sum, lane) => sum + lane.moved.length, 0) - one
  const counts = diff.lanes.reduce((sum, lane) => sum + movedCount(lane.counts), 0) + movedCount(diff.job)
  const shared = diff.lanes.reduce((sum, lane) => sum + lane.shared, 0)
  const scope = `${shared} cell(s) on ${diff.lanes.length} lane(s) in both`
  return [
    `campaign run ${headOf(diff.before)}`,
    `     against ${headOf(diff.after)}`,
    ...(diff.before.capture === diff.after.capture
      ? []
      : [`note: two campaigns, ${diff.before.capture} and ${diff.after.capture}: only cells of the same name compare`]),
    ...diff.lanes.flatMap((lane) => [
      `lane ${lane.lane}: ${lane.shared} cell(s) in both, ${lane.moved.length} differ`,
      ...lane.moved.flatMap((cell) => cellLines(cell, diff.before.id, diff.after.id)),
      ...countLines(lane.counts)
    ]),
    ...(diff.job.length === 0 ? [] : ["job:", ...countLines(diff.job)]),
    isSame(diff)
      ? `DIFF same (${scope})`
      : `DIFF moved (${moved} cell(s) moved, ${one} on one side only, ${counts} count(s) moved; ${scope})`
  ]
}
