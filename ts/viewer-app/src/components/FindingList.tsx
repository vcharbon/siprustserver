/**
 * Findings, one line each: rule · chip · party · step · the short difference,
 * in columns shared by every row of the list. The full text is the row's
 * tooltip and opens under it on a click of the ▸.
 */
import type { JSX } from "react"
import { useState } from "react"

export interface FindingChip {
  readonly label: string
  /** The badge class suffix: `gating`, `source`, `advisory`, … */
  readonly tone: string
  readonly title: string
}

export interface Finding {
  readonly rule: string
  readonly chip?: FindingChip | undefined
  readonly party?: string | undefined
  readonly step?: string | undefined
  /** Where no step names the place: the audit's message number, say. */
  readonly at?: string | undefined
  readonly text: string
  /** The whole finding, where it says more than `text`. */
  readonly full?: string | undefined
}

/** The step chip: the finding's address on every ladder that draws it. */
export const StepLink = (
  { step, onPickStep }: { step: string; onPickStep: ((step: string) => void) | undefined }
): JSX.Element =>
  onPickStep === undefined
    ? <code className="verdict-step">{step}</code>
    : (
      <button type="button" className="step-link verdict-step" title={`${step} on the ladders`} onClick={() => onPickStep(step)}>
        {step}
      </button>
    )

const Row = (
  { finding, onPickStep }: { finding: Finding; onPickStep: ((step: string) => void) | undefined }
): JSX.Element => {
  const [open, setOpen] = useState(false)
  const more = finding.full !== undefined && finding.full !== finding.text
  return (
    <div className={`finding${open ? " open" : ""}`} title={finding.full ?? finding.text}>
      <span className="finding-toggle">
        {more && (
          <button type="button" className="finding-expand" aria-expanded={open} onClick={() => setOpen(!open)}>
            {open ? "▾" : "▸"}
          </button>
        )}
      </span>
      <code className="verdict-tag finding-rule">{finding.rule}</code>
      <span className="finding-chip">
        {finding.chip !== undefined && (
          <span className={`rfc-badge rfc-badge-${finding.chip.tone}`} title={finding.chip.title}>
            {finding.chip.label}
          </span>
        )}
      </span>
      <span className="finding-party mono">{finding.party ?? ""}</span>
      <span className="finding-step">
        {finding.step !== undefined
          ? <StepLink step={finding.step} onPickStep={onPickStep} />
          : finding.at !== undefined && <span className="muted mono">{finding.at}</span>}
      </span>
      <span className="finding-text">{finding.text}</span>
      {open && more && <pre className="finding-full">{finding.full}</pre>}
    </div>
  )
}

export const FindingList = (
  { findings, onPickStep }: {
    findings: readonly Finding[]
    onPickStep?: ((step: string) => void) | undefined
  }
): JSX.Element => (
  <div className="finding-list">
    {findings.map((finding, index) => (
      // The list's own order and nothing else identifies a row.
      <Row key={index} finding={finding} onPickStep={onPickStep} />
    ))}
  </div>
)
