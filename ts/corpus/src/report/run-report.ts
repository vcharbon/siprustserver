/**
 * The RUN REPORT of one run set (`@sip/contracts` `run-report.ts`,
 * `sip.run-report/1`): how its calls ended, each as the compare report's own
 * cell (`compare-report.ts`), so a call reads the same in a report and in a
 * comparison; and the text that renders it.
 *
 * A set read off the store's run rows is {@link fromRuns}; a cited run whose
 * row the store no longer holds is {@link fromSummary}, off the summary a
 * citation kept — its cells carry verdicts, steps and main errors and no
 * bless class. No ladder is read: a report says how calls ended, and its links
 * lead to each call's evidence.
 */
import { CompareReport, RunReport } from "@sip/contracts"
import { Compare, type Model, type RunSets, type RunSummaries } from "@sip/store"
import * as Report from "./compare-report.js"

/** Which of the three ways a call ended. */
const endOf = (cell: CompareReport.Cell): "passed" | "failed" | "skipped" =>
  cell.skipped !== null ? "skipped" : cell.passed ? "passed" : "failed"

const countsOf = (calls: ReadonlyArray<RunReport.Call>): RunReport.Counts => {
  const ends = calls.map((call) => endOf(call.cell))
  const of = (end: string): number => ends.filter((one) => one === end).length
  return { calls: calls.length, passed: of("passed"), failed: of("failed"), skipped: of("skipped") }
}

const headlineOf = (counts: RunReport.Counts): string =>
  `${counts.calls} call${counts.calls === 1 ? "" : "s"}: ${counts.passed} passed, ${counts.failed} failed, ` +
  `${counts.skipped} skipped`

const codeOf = (code: Model.Provenance): ReadonlyArray<CompareReport.CellComponent> =>
  code.map((component) => ({ name: component.name, sha: component.sha }))

const callsOf = (viewer: string, cells: ReadonlyArray<Compare.Cell>): ReadonlyArray<RunReport.Call> =>
  cells.map((cell) => ({
    capture: cell.capture,
    caseId: cell.caseId,
    cell: Report.cellJson(viewer, cell, Report.NO_EVIDENCE)
  }))

export interface FromRuns {
  readonly selector: string
  /** The newest runs the set holds, as many as the reader lists. */
  readonly runs: ReadonlyArray<RunSets.RunRow>
  /** How many runs the set holds. */
  readonly total: number
  /** The set's cells: each call at its latest run of the set. */
  readonly side: Compare.Side
  /** The viewer every link points at, `scheme://host:port`. */
  readonly viewer: string
}

/** The report of a set the store holds the rows of. */
export const fromRuns = (input: FromRuns): RunReport.RunReport => {
  const calls = callsOf(input.viewer, input.side.cells)
  const counts = countsOf(calls)
  return {
    schema_version: RunReport.SCHEMA_VERSION,
    selector: input.selector,
    source: "runs",
    headline: headlineOf(counts),
    runs: input.runs.map((run) => ({
      id: run.id,
      capture: run.capture,
      lane: run.lane,
      setKind: run.setKind,
      status: run.status,
      env: run.env,
      branch: run.branch,
      code: codeOf(run.code),
      sweepId: run.sweepId,
      jobId: run.jobId,
      startedAt: run.startedAt,
      finishedAt: run.finishedAt,
      cases: run.cases,
      failed: run.failed,
      citedBy: []
    })),
    total: input.total,
    side: Report.sideJson(input.side),
    counts,
    calls
  }
}

/** A summarised cell as the compare report's cell reads it: no bundle, no lane hash, no document, no deltas. */
const cellOfSummary = (run: RunSummaries.RunSummary, cell: RunSummaries.CellSummary): Compare.Cell => ({
  capture: cell.capture,
  caseId: cell.caseId,
  runId: run.runId,
  lane: cell.lane,
  env: cell.env,
  branch: cell.branch,
  laneHash: null,
  prepId: run.prepId,
  bundlePath: "",
  passed: cell.passed,
  verdict: cell.verdict,
  skipped: cell.skipped,
  failedStep: cell.failedStep,
  documentHash: null,
  mainError: cell.mainError,
  deltas: null,
  code: cell.code
})

/** The report of a cited run whose row is gone, off the summary a citation kept. */
export const fromSummary = (
  selector: string,
  kept: RunSummaries.Summarised,
  viewer: string
): RunReport.RunReport => {
  const cells = kept.cells.map((cell) => cellOfSummary(kept.run, cell))
  const calls = callsOf(viewer, cells)
  const counts = countsOf(calls)
  const run = kept.run
  return {
    schema_version: RunReport.SCHEMA_VERSION,
    selector,
    source: "summaries",
    headline: headlineOf(counts),
    runs: [{
      id: run.runId,
      capture: run.capture,
      lane: run.lane,
      setKind: run.setKind,
      status: run.status,
      env: run.env,
      branch: run.branch,
      code: codeOf(run.code),
      sweepId: run.sweepId,
      jobId: run.jobId,
      startedAt: run.startedAt,
      finishedAt: run.finishedAt,
      cases: run.cases,
      failed: run.failed,
      citedBy: run.citedBy
    }],
    total: 1,
    side: Report.sideJson({ selector, runs: 1, sweeps: [], cells, elsewhere: Compare.NOWHERE_ELSE }),
    counts,
    calls
  }
}

const short = (sha: string | null): string => (sha === null ? "unknown" : sha.slice(0, 10))

const runLine = (run: RunReport.Run): string =>
  `  run${run.id} ${run.capture} on ${run.lane}: ${run.setKind}, ${run.status}; ` +
  `env ${run.env ?? "unknown"}@${run.branch ?? "unknown"}; ` +
  (run.code.length === 0 ? "code unknown" : run.code.map((one) => `${one.name} ${short(one.sha)}`).join(", ")) +
  `; started ${run.startedAt}${run.finishedAt === null ? "" : `, finished ${run.finishedAt}`}` +
  `; ${run.cases} case(s), ${run.failed} failed` +
  (run.sweepId === null ? "" : `; sweep ${run.sweepId}`) +
  (run.jobId === null ? "" : `; job ${run.jobId}`)

/** The document as text: the headline, where it was read from, each run, then each call, at most `limit`. */
export const render = (doc: RunReport.RunReport, limit: number): string => {
  const source = doc.source === "runs"
    ? []
    : doc.runs.map((run) =>
      `  run ${run.id}'s row is gone; the summary a citation kept ` +
      `(${run.citedBy.length === 0 ? "no citation named" : run.citedBy.join(", ")}) stands for it`
    )
  const calls = doc.calls.slice(0, limit).flatMap((call) => [
    "",
    `== ${call.caseId}`,
    Report.cellLine("", call.cell),
    `  study ${call.cell.links.study}`,
    `  text  ${call.cell.links.text}`
  ])
  return [
    `report ${doc.selector}: ${doc.headline}`,
    ...source,
    ...doc.runs.map(runLine),
    ...(doc.total > doc.runs.length ? [`  … ${doc.total - doc.runs.length} more run(s)`] : []),
    ...calls,
    ...(doc.calls.length > limit ? ["", `… ${doc.calls.length - limit} more call(s) in the JSON`] : [])
  ].join("\n")
}
