/**
 * The post-run RFC audit, as the cell's `rfc.json` states it.
 *
 * The audit is NOT the verdict: a cell whose verdict says `ok` and whose
 * classification says `passed` is still red when the audit gates, and then the
 * audit is the only thing on the page that says why. So it is drawn beside the
 * verdict rather than folded into it, and never dropped.
 *
 * A gating finding is the SUT's, and turns the cell red; a source fault is a
 * scripted party's violation the document does not state — the replayed party
 * repeats a captured message where, in this run, the RFC no longer allows it —
 * information that turns
 * nothing red; a scripted party's finding the document states (the reference
 * shows it) or could not judge is information; an advisory one is a rule the
 * lane records and refuses to fail on; a relayed one is the SUT's, cancelled
 * as caused by the party the document states committing it. Gating findings
 * and source faults are drawn in sections of their own because collapsing them
 * into the rest would hide the one that fails the cell and the one whose
 * source broke the RFC — and for the same reason an audit that found nothing
 * states that it ran.
 *
 * `rfc.json` reaches the browser untyped, so every field is read through a
 * guard: a finding this pane cannot read is listed as raw JSON rather than
 * dropped.
 */
import type { JSX } from "react"
import { shortDifference, stepNamed } from "../finding-text.ts"
import { type Finding as FindingLine, type FindingChip, FindingSection } from "./FindingList.tsx"

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

const str = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined)

const num = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined

const list = (value: unknown): ReadonlyArray<unknown> => (Array.isArray(value) ? value : [])

/** One rule the audit found broken, and who it was charged to. */
interface Finding {
  readonly rule: string
  readonly detail: string
  /** The party the audit holds responsible; the lane's own address where it says none. */
  readonly charged: string
  /** The scripted actor behind `charged`, where the audit names one. */
  readonly actor: string | undefined
  /** The lane the traffic was seen on. */
  readonly lane: string | undefined
  /** The message the finding was raised at, as the audit numbers them. */
  readonly offending: number | undefined
  /** This finding is the SUT's, and why the cell is red. */
  readonly gating: boolean
  /** A scripted party's violation the document does not state: information, never the SUT's. */
  readonly sourceFault: boolean
  /** The rule is recorded and never failed on. */
  readonly advisory: boolean
  /** The document's statement that cancelled it, where one did. */
  readonly cancelledBy: string | undefined
  /** The party whose violation the system under test relayed onward, where the document states both. */
  readonly causedBy: string | undefined
  /** Why a scripted party's finding was judged neither way, where the document never read its captured trace. */
  readonly unjudged: string | undefined
}

const readFinding = (value: unknown): Finding => {
  if (!isRecord(value)) {
    return {
      rule: "unreadable finding",
      detail: JSON.stringify(value) ?? "",
      charged: "?",
      actor: undefined,
      lane: undefined,
      offending: undefined,
      gating: false,
      sourceFault: false,
      advisory: false,
      cancelledBy: undefined,
      causedBy: undefined,
      unjudged: undefined
    }
  }
  return {
    rule: str(value.rule) ?? "rfc-violation",
    detail: str(value.detail) ?? "",
    charged: str(value.charged) ?? "?",
    actor: str(value.actor),
    lane: str(value.lane),
    offending: num(value.offending),
    gating: value.gating === true,
    sourceFault: value.source_fault === true,
    advisory: value.advisory === true,
    cancelledBy: str(value.cancelled_by),
    causedBy: str(value.caused_by),
    unjudged: str(value.unjudged)
  }
}

/** What a finding is, in a word: its chip, and the one line that says what the chip means. */
const weight = (finding: Finding): FindingChip =>
  finding.gating
    ? { label: "gates", tone: "gating", title: "the SUT's: why the cell is red" }
    : finding.sourceFault
    ? { label: "source fault", tone: "source", title: "a scripted party's violation the document does not state" }
    : finding.causedBy !== undefined
    ? { label: "relayed", tone: "relayed", title: `relayed by the SUT, committed by ${finding.causedBy}` }
    : finding.actor !== undefined && finding.cancelledBy !== undefined
    ? { label: "ref capture", tone: "party", title: "in the reference capture; not gating" }
    : finding.unjudged !== undefined
    ? { label: "scripted · unjudged", tone: "party", title: "a scripted party's violation the document could not judge" }
    : finding.advisory
    ? { label: "advisory", tone: "advisory", title: "advisory rule: recorded, gates nothing" }
    : { label: "not gated", tone: "bound", title: "binding rule, charged to a party this lane does not gate on" }

/** The finding's whole text: every coordinate the line leaves out. */
const fullText = (finding: Finding): string =>
  [
    `charged ${finding.charged}${finding.actor === undefined ? "" : ` (${finding.actor})`}`,
    ...(finding.lane === undefined ? [] : [`on ${finding.lane}`]),
    ...(finding.offending === undefined ? [] : [`message ${finding.offending}`]),
    ...(finding.causedBy === undefined ? [] : [`caused by ${finding.causedBy}`]),
    ...(finding.cancelledBy === undefined ? [] : [`cancelled by ${finding.cancelledBy}`]),
    ...(finding.unjudged === undefined ? [] : [`unjudged: ${finding.unjudged}`]),
    ...(finding.detail === "" ? [] : [finding.detail])
  ].join("\n")

const line = (finding: Finding): FindingLine => ({
  rule: finding.rule,
  chip: weight(finding),
  party: finding.actor ?? finding.charged,
  step: stepNamed(finding.cancelledBy),
  at: finding.offending === undefined ? undefined : `m${finding.offending}`,
  text: shortDifference(finding.detail),
  full: fullText(finding)
})

/** One group of the audit's findings, as a finding section. */
const Section = (
  { tone, title, findings, fold, onPickStep }: {
    tone: string
    title: string
    findings: ReadonlyArray<Finding>
    fold?: boolean
    onPickStep: ((step: string) => void) | undefined
  }
): JSX.Element | null => (
  <FindingSection
    className={`rfc-audit rfc-audit-${tone}`}
    title={title}
    findings={findings.map(line)}
    fold={fold}
    onPickStep={onPickStep}
  />
)

/**
 * The audit's own line for a violation the verdict lists, where the audit
 * cancelled one of its findings by that statement: its short difference and
 * its whole text.
 */
export const auditLineOf = (
  rfc: unknown,
  violation: { readonly rule: string; readonly step: string; readonly emitter: string }
): { readonly text: string; readonly full: string } | undefined => {
  if (!isRecord(rfc)) return undefined
  const statement = `${violation.rule} at ${violation.step} by ${violation.emitter}`
  const finding = list(rfc.findings).map(readFinding).find((one) => one.cancelledBy?.includes(statement) === true)
  return finding === undefined ? undefined : { text: shortDifference(finding.detail), full: fullText(finding) }
}

export const RfcAuditPane = (
  { rfc, onPickStep }: {
    rfc: unknown
    /** Puts a finding's step on the ladders; absent where there are none. */
    onPickStep?: ((step: string) => void) | undefined
  }
): JSX.Element | null => {
  if (!isRecord(rfc)) return null
  const status = str(rfc.status) ?? "unknown"
  const findings = list(rfc.findings).map(readFinding)

  // An audit that ran and found nothing is a RESULT. Drawn as nothing it would
  // read as an audit that never ran, which is the opposite claim.
  if (findings.length === 0) {
    return (
      <section className="verdict-section rfc-audit rfc-audit-clean">
        <h4>rfc audit <span className="count">{status === "audited" ? "✓ none" : status}</span></h4>
      </section>
    )
  }

  return (
    <>
      <Section
        tone="gating"
        title="rfc audit · gating"
        findings={findings.filter((finding) => finding.gating)}
        onPickStep={onPickStep}
      />
      <Section
        tone="source"
        title="rfc audit · source fault"
        findings={findings.filter((finding) => !finding.gating && finding.sourceFault)}
        onPickStep={onPickStep}
      />
      <Section
        tone="advisory"
        title="rfc audit · not gating"
        findings={findings.filter((finding) => !finding.gating && !finding.sourceFault)}
        fold
        onPickStep={onPickStep}
      />
      {status !== "audited" && <p className="muted">audit status: {status}</p>}
    </>
  )
}
