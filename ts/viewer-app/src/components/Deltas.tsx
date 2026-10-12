/**
 * The delta chips: what the confrontation made of the replay's DIFFERENCES from
 * the capture, and nothing else.
 *
 * They are not a run outcome and must never be read as one. A replay that
 * aborted at its third step diverges from almost nothing, so it scores every
 * delta accepted and still failed. `failed step` beside them is the outcome;
 * these say what the surviving comparison cost.
 */
import type { JSX } from "react"

export interface DeltaCounts {
  readonly accepted: number
  readonly knownBug: number
  readonly unlisted: number
  readonly unknown: number
  readonly compared: number
  readonly unreferenced: number
  /** Cases that carried a classification at all; 0 = the counts say nothing. */
  readonly classified: number
}

/** Reads a `classification.json` blob, whose fields are snake_case. */
export const deltasOfClassification = (value: unknown): DeltaCounts | undefined => {
  if (typeof value !== "object" || value === null) return undefined
  const record = value as Record<string, unknown>
  const num = (field: string): number => typeof record[field] === "number" ? record[field] : 0
  return {
    accepted: num("accepted"),
    knownBug: num("known_bug"),
    unlisted: num("unlisted"),
    unknown: num("unknown"),
    compared: num("compared"),
    unreferenced: num("unreferenced"),
    classified: 1
  }
}

/** The counts, `unlisted`/`unknown` first because those are the ones that cost. */
export const Deltas = ({ counts, detail }: { counts: DeltaCounts; detail?: boolean }): JSX.Element => {
  if (counts.classified === 0) return <span className="muted">no classification</span>
  return (
    <span className="delta-chips">
      <span className="tag class-unknown">{counts.unknown} unknown</span>
      <span className="tag class-unlisted">{counts.unlisted} unlisted</span>
      <span className="tag class-known-bug">{counts.knownBug} known-bug</span>
      <span className="tag class-accepted">{counts.accepted} accepted</span>
      {detail === true && (
        <span className="muted">
          {counts.compared} compared · {counts.unreferenced} unreferenced
        </span>
      )}
    </span>
  )
}

/**
 * The steps a case died at — the defect, stated as such. Where the caller can
 * act on a step the chip becomes the way in: naming the defect and not being
 * able to reach it is the gap `onPick` closes.
 */
export const FailedSteps = (
  { steps, onPick }: {
    steps: readonly string[]
    /** Focus the step on this page's ladders. */
    onPick?: ((step: string) => void) | undefined
  }
): JSX.Element | null =>
  steps.length === 0 ? null : (
    <span className="failed-steps" title="the replay stopped here">
      {steps.map((step) =>
        onPick === undefined
          ? <span key={step} className="tag failed-step">died at {step}</span>
          : (
            <button key={step} type="button" className="tag failed-step" onClick={() => onPick(step)}>
              died at {step}
            </button>
          )
      )}
    </span>
  )
