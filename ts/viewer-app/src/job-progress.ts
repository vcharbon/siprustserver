/**
 * The progress bar's arithmetic (§6 J3), pure: what fraction of a job's
 * units has landed, how the landed part splits into passed, failed and
 * skipped, how much of the plan is gone, and the rate and the time left off
 * the row's own instants. A job with nothing planned has an empty bar, never
 * a division.
 */
import type { JobProgress } from "./api.ts"

/** One coloured run of the bar, as a percentage of its width. */
export interface Segment {
  readonly kind: "passed" | "failed" | "skipped" | "landed" | "gone"
  readonly percent: number
  readonly count: number
}

export interface Bar {
  readonly segments: readonly Segment[]
  /** Landed units over the plan, as a percentage. */
  readonly percent: number
  readonly landed: number
  readonly total: number
  /** Cells the resolution found no case for: they never land. */
  readonly gone: number
}

const percentOf = (count: number, total: number): number => (total <= 0 ? 0 : (100 * count) / total)

/**
 * The results split the landed part where they are counted (a replay's
 * `case_results`, written as each capture's row closes); the landed cells
 * with no result yet ride as `landed`, so the bar moves per batch and colours
 * per capture.
 */
export const barOf = (progress: JobProgress): Bar => {
  const { total } = progress
  const gone = progress.goneRefused + progress.goneMissing
  const resulted = progress.passed + progress.failed + progress.skipped
  const unresulted = Math.max(0, progress.landed - resulted)
  const segment = (kind: Segment["kind"], count: number): Segment => ({ kind, count, percent: percentOf(count, total) })
  const segments = [
    segment("passed", progress.passed),
    segment("failed", progress.failed),
    segment("skipped", progress.skipped),
    segment("landed", unresulted),
    segment("gone", gone)
  ].filter((one) => one.count > 0)
  return { segments, percent: percentOf(progress.landed, total), landed: progress.landed, total, gone }
}

export interface Pace {
  /** Units per second over the time the job has run. */
  readonly rate: number
  readonly elapsedSeconds: number
  /** Seconds to the end at that rate; undefined where nothing has landed yet or the job has ended. */
  readonly leftSeconds: number | undefined
}

/** The rate off the row's instants (`now` where the job has not finished), as the job command prints it. */
export const paceOf = (
  progress: JobProgress,
  startedAt: string | null,
  finishedAt: string | null,
  now: number
): Pace | undefined => {
  if (startedAt === null) return undefined
  const from = new Date(startedAt).getTime()
  const to = finishedAt === null ? now : new Date(finishedAt).getTime()
  const elapsedSeconds = Math.max(0, (to - from) / 1000)
  const rate = elapsedSeconds > 0 ? progress.landed / elapsedSeconds : 0
  const leftSeconds = finishedAt === null && rate > 0 ? Math.round(progress.pending / rate) : undefined
  return { rate, elapsedSeconds, leftSeconds }
}

/** `1h 02m`, `4m 05s`, `12s`: enough precision for a bar's caption. */
export const durationText = (seconds: number): string => {
  const whole = Math.max(0, Math.round(seconds))
  const h = Math.floor(whole / 3600)
  const m = Math.floor((whole % 3600) / 60)
  const s = whole % 60
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m`
  if (m > 0) return `${m}m ${String(s).padStart(2, "0")}s`
  return `${s}s`
}

/** The unit a job counts: cells for a replay, runs for a delete, captures otherwise. */
export const unitOf = (kind: string): string => (kind === "replay" ? "cell" : kind === "delete" ? "run" : "capture")

/** A list row's counts as a progress: what the compact bar draws — landed, red among them, gone. */
export const progressOfListed = (
  row: { readonly landed: number; readonly total: number; readonly failed: number; readonly gone: number }
): JobProgress => ({
  total: row.total,
  landed: row.landed,
  pending: Math.max(0, row.total - row.landed - row.gone),
  goneRefused: row.gone,
  goneMissing: 0,
  passed: 0,
  failed: row.failed,
  skipped: 0,
  runs: 0,
  openRuns: 0
})
