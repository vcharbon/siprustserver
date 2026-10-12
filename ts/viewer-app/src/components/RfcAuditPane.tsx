/**
 * The post-run RFC audit, as the cell's `rfc.json` states it.
 *
 * The audit is NOT the verdict: a cell whose verdict says `ok` and whose
 * classification says `passed` is still red when the audit gates, and then the
 * audit is the only thing on the page that says why. So it is drawn beside the
 * verdict rather than folded into it, and never dropped.
 *
 * A gating finding is what turns the cell red; an advisory one is a rule the
 * lane records and refuses to fail on; a relayed one is the SUT's, cancelled
 * as caused by the party the document states committing it. Gating findings
 * are drawn in a section of their own because collapsing them would make a
 * red cell and a clean one read alike — and for the same reason an audit that
 * found nothing states that it ran.
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
  /** This finding is why the cell is red. */
  readonly gating: boolean
  /** The rule is recorded and never failed on. */
  readonly advisory: boolean
  /** The document's statement that cancelled it, where one did. */
  readonly cancelledBy: string | undefined
  /** The party whose violation the system under test relayed onward, where the document states both. */
  readonly causedBy: string | undefined
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
      advisory: false,
      cancelledBy: undefined,
      causedBy: undefined
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
    advisory: value.advisory === true,
    cancelledBy: str(value.cancelled_by),
    causedBy: str(value.caused_by)
  }
}

/**
 * What a non-gating finding is, in a word. A rule that is advisory never fails
 * anyone; a binding rule that does not gate was charged to a party this lane
 * does not hold to it, and both are worth telling apart from the one that did.
 */
const weight = (finding: Finding): { readonly badge: string; readonly title: string } =>
  finding.gating
    ? { badge: "gates", title: "this finding is why the cell is red" }
    : finding.causedBy !== undefined
    ? {
      badge: "relayed",
      title: "the SUT relayed another party's violation, and the document states that party committing it and the captured platform relaying it"
    }
    : finding.advisory
    ? { badge: "advisory", title: "an advisory rule: recorded, failed on by nobody" }
    : { badge: "does not gate", title: "a binding rule, charged to a party this lane does not gate on" }

const FindingRow = ({ finding }: { finding: Finding }): JSX.Element => {
  const { badge, title } = weight(finding)
  return (
    <p className="verdict-row">
      <code className="verdict-tag">{finding.rule}</code>
      <span
        className={`rfc-badge rfc-badge-${
          finding.gating ? "gating" : finding.causedBy !== undefined ? "relayed" : finding.advisory ? "advisory" : "bound"
        }`}
        title={title}
      >
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
      </span>
    </p>
  )
}

export const RfcAuditPane = ({ rfc }: { rfc: unknown }): JSX.Element | null => {
  if (!isRecord(rfc)) return null
  const status = str(rfc.status) ?? "unknown"
  const findings = list(rfc.findings).map(readFinding)
  const gating = findings.filter((finding) => finding.gating)
  const rest = findings.filter((finding) => !finding.gating)

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
      {gating.length > 0 && (
        <section className="verdict-section rfc-audit rfc-audit-gating">
          <h4>rfc audit — gating ({gating.length})</h4>
          <p className="muted">
            the post-run audit of what the wire carried. These are why this cell is red, whatever
            the verdict and the delta sheet say.
          </p>
          {gating.map((finding, index) => (
            // The audit's own order and nothing else identifies a row.
            <FindingRow key={index} finding={finding} />
          ))}
        </section>
      )}
      {rest.length > 0 && (
        <section className="verdict-section rfc-audit rfc-audit-advisory">
          <h4>rfc audit — not gating ({rest.length})</h4>
          <p className="muted">
            recorded, and failed on by nobody: an advisory rule, or a binding one charged to a party
            this lane does not gate on.
          </p>
          {rest.map((finding, index) => (
            // The audit's own order and nothing else identifies a row.
            <FindingRow key={index} finding={finding} />
          ))}
        </section>
      )}
      {status !== "audited" && (
        <p className="muted">the audit states "{status}".</p>
      )}
    </>
  )
}
