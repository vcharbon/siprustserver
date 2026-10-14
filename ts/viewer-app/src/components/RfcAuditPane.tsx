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

/** What a finding is, in a word: its badge, the class that draws it, and what the badge means. */
const weight = (finding: Finding): { readonly badge: string; readonly tone: string; readonly title: string } =>
  finding.gating
    ? { badge: "gates", tone: "gating", title: "the SUT's finding: this is why the cell is red" }
    : finding.sourceFault
    ? {
      badge: "source fault",
      tone: "source",
      title: "a scripted party's violation the document does not state: the replayed party repeats a captured message where, in this run, the RFC no longer allows it"
    }
    : finding.causedBy !== undefined
    ? {
      badge: "relayed",
      tone: "relayed",
      title: "the SUT relayed another party's violation, and the document states that party committing it and the captured platform relaying it"
    }
    : finding.actor !== undefined && finding.cancelledBy !== undefined
    ? {
      badge: "scripted party · the reference shows it",
      tone: "party",
      title: "a scripted party's violation the document states: the captured trace holds the same one"
    }
    : finding.unjudged !== undefined
    ? {
      badge: "scripted party · unjudged",
      tone: "party",
      title: "a scripted party's violation the document could not judge: it never read its captured trace's violations"
    }
    : finding.advisory
    ? { badge: "advisory", tone: "advisory", title: "an advisory rule: recorded, failed on by nobody" }
    : { badge: "does not gate", tone: "bound", title: "a binding rule, charged to a party this lane does not gate on" }

const FindingRow = ({ finding }: { finding: Finding }): JSX.Element => {
  const { badge, tone, title } = weight(finding)
  return (
    <p className="verdict-row">
      <code className="verdict-tag">{finding.rule}</code>
      <span className={`rfc-badge rfc-badge-${tone}`} title={title}>
        {badge}
      </span>
      <span className="verdict-detail">
        charged {finding.charged}
        {finding.actor !== undefined && ` (${finding.actor})`}
        {finding.lane !== undefined && ` · on ${finding.lane}`}
        {finding.offending !== undefined && ` · message ${finding.offending}`}
        {finding.causedBy !== undefined && ` · caused by ${finding.causedBy}`}
        {finding.detail !== "" && ` — ${finding.detail}`}
        {finding.cancelledBy !== undefined && ` · cancelled by ${finding.cancelledBy}`}
        {finding.unjudged !== undefined && ` · unjudged: ${finding.unjudged}`}
      </span>
    </p>
  )
}

/** One titled group of findings, drawn only where it holds one. */
const Section = (
  { tone, title, lead, findings }: {
    tone: string
    title: string
    lead: string
    findings: ReadonlyArray<Finding>
  }
): JSX.Element | null =>
  findings.length === 0 ? null : (
    <section className={`verdict-section rfc-audit rfc-audit-${tone}`}>
      <h4>rfc audit — {title} ({findings.length})</h4>
      <p className="muted">{lead}</p>
      {findings.map((finding, index) => (
        // The audit's own order and nothing else identifies a row.
        <FindingRow key={index} finding={finding} />
      ))}
    </section>
  )

export const RfcAuditPane = ({ rfc }: { rfc: unknown }): JSX.Element | null => {
  if (!isRecord(rfc)) return null
  const status = str(rfc.status) ?? "unknown"
  const findings = list(rfc.findings).map(readFinding)

  // An audit that ran and found nothing is a RESULT. Drawn as nothing it would
  // read as an audit that never ran, which is the opposite claim.
  if (findings.length === 0) {
    return (
      <section className="verdict-section rfc-audit rfc-audit-clean">
        <h4>rfc audit</h4>
        <p className="muted">
          {status === "audited"
            ? "audited, none — every rule this lane checks held on this replay."
            : `the audit states "${status}" and lists no finding.`}
        </p>
      </section>
    )
  }

  return (
    <>
      <Section
        tone="gating"
        title="gating"
        lead="the post-run audit of what the wire carried. These are the SUT's, and why this cell is red, whatever the verdict and the delta sheet say."
        findings={findings.filter((finding) => finding.gating)}
      />
      <Section
        tone="source"
        title="source fault"
        lead="a scripted party's violation the document does not state: the replayed party repeats a captured message where, in this run, the RFC no longer allows it. Information, not the SUT's: the cell is judged on the SUT alone."
        findings={findings.filter((finding) => !finding.gating && finding.sourceFault)}
      />
      <Section
        tone="advisory"
        title="not gating"
        lead="recorded, and failed on by nobody: a scripted party's finding the reference shows or the document could not judge, an advisory rule, or a binding one charged to a party this lane does not gate on."
        findings={findings.filter((finding) => !finding.gating && !finding.sourceFault)}
      />
      {status !== "audited" && <p className="muted">the audit states "{status}".</p>}
    </>
  )
}
