/**
 * A job as the command line prints it: one line in the list, the row and its
 * progress under `show` — the landing rate and what is left read off the
 * row's own instants, so a poll computes nothing the store did not state.
 */
import type { Jobs } from "@sip/store"

const seconds = (from: string, to: string | null): number =>
  Math.max(0, (new Date(to ?? Date.now()).getTime() - new Date(from).getTime()) / 1000)

/** The unit a job counts: cells for a replay, runs for a delete, captures otherwise. */
const unit = (kind: Jobs.Kind): string => (kind === "replay" ? "cell" : kind === "delete" ? "run" : "capture")

/** What a job's landed units did: replayed, removed, or cut. */
const verb = (kind: Jobs.Kind): string =>
  kind === "replay" ? "landed" : kind === "retire" || kind === "delete" ? "removed" : "cut"

/** What a job is, in a few words: a replay names its request and lane; the others what they planned. */
export const what = (job: Jobs.Row): string =>
  `${job.kind}${job.requestId === null ? "" : ` request ${job.requestId}`}` +
  (job.lane === null ? "" : ` on ${job.lane}`) +
  (job.planned === null || job.kind === "replay" ? "" : ` over ${job.planned} ${unit(job.kind)}(s)`)

export const listLine = (job: Jobs.Listed): string =>
  `job ${String(job.id).padStart(4)}  ${job.createdAt}  ${what(job).padEnd(34)}  ${job.status.padEnd(11)}` +
  `${String(job.landed).padStart(6)}/${String(job.total).padEnd(6)} ${verb(job.kind).padEnd(7)}` +
  (job.pid === null ? "" : `  pid ${job.pid}`) +
  (job.waitingOn === null ? "" : `  waiting on ${job.waitingOn}`) +
  (job.stopRequestedAt === null || job.status !== "running" ? "" : "  stopping") +
  (job.restartOf === null ? "" : `  ${job.args["unran"] === true ? "resume" : "again"} of ${job.restartOf}`)

/** The row and its progress as `show` prints them. */
export const showLines = (detail: Jobs.Detail): ReadonlyArray<string> => {
  const { job, progress } = detail
  const lines: Array<string> = [
    `job ${job.id}: ${what(job)} — ${job.status}` +
    (job.exit === null ? "" : ` (exit ${job.exit})`) +
    (job.pid === null ? "" : `  pid ${job.pid}`) +
    (job.startedAt === null ? "" : `  since ${job.startedAt}`) +
    (job.finishedAt === null ? "" : `  until ${job.finishedAt}`) +
    `  by ${job.createdBy}`
  ]
  if (job.kind === "replay") {
    lines.push(
      `  cells: ${progress.total} planned, ${progress.landed} landed, ${progress.pending} pending, ` +
        `${progress.goneRefused + progress.goneMissing} gone ` +
        `(${progress.goneRefused} refused, ${progress.goneMissing} not in this cut)`,
      `  results: ${progress.passed} passed, ${progress.failed} failed, ${progress.skipped} skipped ` +
        `over ${progress.runs} run(s), ${progress.openRuns} open`
    )
  } else {
    lines.push(
      `  ${unit(job.kind)}s: ${progress.total} planned, ${progress.landed} ${verb(job.kind)}, ${progress.pending} pending`
    )
  }
  if (job.batchAt !== null && job.batches !== null) lines.push(`  batch: ${job.batchAt}/${job.batches}`)
  if (job.stopRequestedAt !== null) {
    lines.push(`  stop requested: ${job.stopRequestedAt} (grace ${job.stopGrace ?? "?"} s)`)
  }
  if (job.restartOf !== null) lines.push(`  restart of: job ${job.restartOf} (${job.args["unran"] === true ? "resume" : "again"})`)
  if (job.startedAt !== null && progress.landed > 0) {
    const elapsed = seconds(job.startedAt, job.finishedAt)
    const rate = elapsed > 0 ? progress.landed / elapsed : 0
    const left = rate > 0 ? Math.round(progress.pending / rate) : undefined
    lines.push(
      `  rate: ${rate.toFixed(2)} ${unit(job.kind)}(s)/s over ${Math.round(elapsed)} s` +
      (job.finishedAt === null && left !== undefined ? `, ~${left} s left` : "")
    )
  }
  if (job.waitingOn !== null) lines.push(`  waiting on: ${job.waitingOn}`)
  if (job.sweepId !== null) lines.push(`  sweep: ${job.sweepId}`)
  if (job.logPath !== null) lines.push(`  log: ${job.logPath}`)
  if (job.hooksPath !== null) lines.push(`  hooks: ${job.hooksPath}`)
  return lines
}

