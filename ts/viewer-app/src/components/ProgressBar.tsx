/**
 * A job's progress as a bar (§6 J3): landed over planned, the landed part
 * coloured by what the results say so far, the gone cells greyed at the end,
 * and the counts beside it. The bar reads the store's counts alone; the
 * arithmetic is `job-progress.ts`.
 */
import type { JSX } from "react"
import type { JobProgress } from "../api.ts"
import { barOf, durationText, paceOf } from "../job-progress.ts"

export interface Props {
  readonly progress: JobProgress
  /** The unit the job counts: cells, captures or runs. */
  readonly unit: string
  readonly startedAt?: string | null | undefined
  readonly finishedAt?: string | null | undefined
  /** Compact: the bar and `landed/total` alone, for a list row. */
  readonly compact?: boolean | undefined
}

export const ProgressBar = ({ progress, unit, startedAt, finishedAt, compact }: Props): JSX.Element => {
  const bar = barOf(progress)
  const pace = startedAt === undefined ? undefined : paceOf(progress, startedAt, finishedAt ?? null, Date.now())
  const title = `${bar.landed} of ${bar.total} ${unit}(s) landed · ${progress.passed} passed, ${progress.failed} failed, ` +
    `${progress.skipped} skipped · ${bar.gone} gone`
  return (
    <div className={`progress${compact ? " compact" : ""}`} title={title}>
      <div className="progress-bar" role="progressbar" aria-valuemin={0} aria-valuemax={bar.total} aria-valuenow={bar.landed}>
        {bar.segments.map((segment) => (
          <span key={segment.kind} className={`progress-${segment.kind}`} style={{ width: `${segment.percent}%` }} />
        ))}
      </div>
      <span className="progress-counts mono">
        {bar.landed}/{bar.total}
        {!compact && <span className="muted"> {unit}(s)</span>}
        {progress.failed > 0 && <span className="call-failed"> · {progress.failed} red</span>}
        {bar.gone > 0 && <span className="muted"> · {bar.gone} gone</span>}
        {!compact && progress.skipped > 0 && <span className="muted"> · {progress.skipped} skipped</span>}
        {!compact && pace !== undefined && pace.rate > 0 && (
          <span className="muted">
            {" · "}{pace.rate.toFixed(2)}/s over {durationText(pace.elapsedSeconds)}
            {pace.leftSeconds !== undefined && `, ~${durationText(pace.leftSeconds)} left`}
          </span>
        )}
      </span>
    </div>
  )
}
