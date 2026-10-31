/**
 * A run verdict, read as six lists rather than one blob: the failures the
 * document DECLARED (§11.2), the divergences a negative case CARRIED beside
 * them, the script a run that could not GO ON ended and the close that ended its
 * call (§14 — it appears on failed positive runs too), what failed anyway, the
 * RFC violations the document declares, and the classified checks that were
 * evaluated on a lane they do not gate on.
 *
 * The five that are not `failures` are the point of this pane. A declared
 * failure is a negative case succeeding — `ok-negative`, a run that passed BY
 * FAILING — and it reads as neither a pass nor a failure; a carried divergence
 * is a real wire difference the negative case did not turn red for, so it is
 * shown rather than swallowed; an abandoned script is steps that never ran, so
 * it is stated rather than left to be inferred from a short step list; a
 * violation a scripted peer emits is what the case exists to reproduce, so it is
 * stated loudly and never buried in a JSON dump; an informative finding is a
 * real miss the run did not turn red for, so it must never read like a failure
 * and never like a pass either.
 *
 * `verdict.json` reaches the browser untyped, so every field is read through a
 * guard: a shape this pane does not recognize falls back to the raw JSON below
 * rather than rendering something invented.
 */
import type { JSX } from "react"
import { type Finding, type FindingChip, FindingList, FindingSection, StepLink } from "./FindingList.tsx"
import { auditLineOf } from "./RfcAuditPane.tsx"

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

const str = (value: unknown): string | undefined =>
  typeof value === "string" ? value : undefined

const list = (value: unknown): ReadonlyArray<unknown> => (Array.isArray(value) ? value : [])

/** One declared RFC violation, as the verdict lists it. */
interface Violation {
  readonly rule: string
  readonly step: string
  readonly emitter: string
  readonly gating: boolean
}

const readViolation = (value: unknown): Violation | undefined => {
  if (!isRecord(value)) return undefined
  const rule = str(value.rule)
  const step = str(value.step)
  const emitter = str(value.emitter)
  if (rule === undefined || step === undefined || emitter === undefined) return undefined
  return { rule, step, emitter, gating: value.gating === true }
}

/** One `must_fail` declaration, against what the run produced for it. */
interface Declared {
  readonly failure: string
  readonly step: string
  readonly derivedFrom: string
  /** The failure the gate raised. Absent where no expect was armed for it. */
  readonly observed: unknown
  /** The arrival as the recording holds it, where the gate raised none. */
  readonly recorded: string | undefined
}

const readDeclared = (value: unknown): Declared | undefined => {
  if (!isRecord(value)) return undefined
  const failure = str(value.failure)
  const step = str(value.step)
  const derivedFrom = str(value.derived_from)
  if (failure === undefined || step === undefined || derivedFrom === undefined) return undefined
  return { failure, step, derivedFrom, observed: value.observed, recorded: str(value.recorded) }
}

/** One act of the generic close: what a scripted leg emitted, and why. */
interface CloseAct {
  readonly leg: string
  readonly owed: string
  readonly sent: string
}

/** A script a declared divergence ended, and the close that ended its call. */
interface Abandoned {
  readonly leg: string
  readonly step: string | undefined
  readonly pending: ReadonlyArray<string>
  readonly closed: ReadonlyArray<CloseAct>
}

const readAbandoned = (value: unknown): Abandoned | undefined => {
  if (!isRecord(value)) return undefined
  const leg = str(value.leg)
  if (leg === undefined) return undefined
  const closed = list(value.closed).flatMap((act) => {
    if (!isRecord(act)) return []
    const actLeg = str(act.leg)
    const owed = str(act.owed)
    const sent = str(act.sent)
    return actLeg === undefined || owed === undefined || sent === undefined
      ? []
      : [{ leg: actLeg, owed, sent }]
  })
  const pending = list(value.pending).flatMap((step) => {
    const id = str(step)
    return id === undefined ? [] : [id]
  })
  return { leg, step: str(value.step), pending, closed }
}

/**
 * What a status means where the word alone does not say it. `ok-negative` is
 * the one that must never be read as plain green: the run FAILED, and failing
 * is what the document asked of it.
 */
const statusNote = (status: string): string | undefined =>
  status === "ok-negative" ? "negative case, failed as declared" : undefined

/** One classified check that did not hold and did not gate. */
interface Informative {
  readonly checkClass: string
  readonly finding: unknown
}

const readInformative = (value: unknown): Informative | undefined => {
  if (!isRecord(value)) return undefined
  const checkClass = str(value.class)
  return checkClass === undefined ? undefined : { checkClass, finding: value.finding }
}

/** `step "s6"` — how an informative finding names the step it was taken at. */
const stepOfSite = (site: unknown): string | undefined =>
  /^step "([^"]+)"$/.exec(str(site) ?? "")?.[1]

/**
 * A failure's own tag, the step it was raised at, and its other fields: one
 * line joined by ` · `, the full text one field per line. The step is lifted
 * out because it is an ADDRESS: it puts the finding on the ladders.
 */
const failureLine = (value: unknown): Finding => {
  if (!isRecord(value)) return { rule: "failure", text: JSON.stringify(value) ?? "" }
  const tag = str(value.failure) ?? "failure"
  const step = str(value.step) ?? stepOfSite(value.site)
  const fields = Object.entries(value)
    .filter(([key]) => key !== "failure" && key !== "step" && !(key === "site" && step !== undefined))
    .map(([key, entry]) => `${key}: ${typeof entry === "string" ? entry : JSON.stringify(entry)}`)
  return { rule: tag, step, text: fields.join(" · "), full: fields.join("\n") }
}

/** A section head: a noun and a count. */
const Head = ({ title, count }: { title: string; count: number }): JSX.Element => (
  <h4>{title} <span className="count">{count}</span></h4>
)

const DECLARED: FindingChip = { label: "must fail", tone: "bound", title: "the failure the document declares its run must produce" }

const declaredLine = (d: Declared): Finding => {
  const seen = d.observed !== undefined
    ? failureLine(d.observed)
    : undefined
  const text = seen !== undefined
    ? `${seen.rule}${seen.text === "" ? "" : ` · ${seen.text}`}`
    : d.recorded !== undefined
    ? `recorded, no expect armed: ${d.recorded}`
    : "✗ not produced"
  return {
    rule: d.failure,
    chip: DECLARED,
    step: d.step,
    links: seen?.step === undefined ? undefined : [seen.step],
    text,
    full: `derived from ${d.derivedFrom}\n${text}${seen?.full === undefined ? "" : `\n${seen.full}`}`
  }
}

export const VerdictPane = (
  { verdict, rfc, onPickStep }: {
    verdict: unknown
    /** The cell's `rfc.json`: the audit's line for each violation the verdict lists. */
    rfc?: unknown
    /** Puts a finding's step on the ladders; absent where there are none. */
    onPickStep?: ((step: string) => void) | undefined
  }
): JSX.Element | null => {
  if (!isRecord(verdict)) return null
  const status = str(verdict.status) ?? "unknown"
  const failures = list(verdict.failures)
  const violations = list(verdict.rfc_violations).map(readViolation).filter((v) => v !== undefined)
  const informative = list(verdict.informative).map(readInformative).filter((i) => i !== undefined)
  const declared = list(verdict.must_fail).map(readDeclared).filter((d) => d !== undefined)
  const tolerated = list(verdict.tolerated)
  const abandoned = readAbandoned(verdict.abandoned)
  const note = statusNote(status)

  return (
    <div className="verdict-pane">
      <p className={`verdict-status verdict-${status}`}>
        {status}
        <span className="muted"> · {str(verdict.lane) ?? "?"}</span>
        {note !== undefined && <span className="muted"> · {note}</span>}
      </p>

      {declared.length > 0 && (
        <section className="verdict-section verdict-declared">
          <Head title="must fail" count={declared.length} />
          <FindingList findings={declared.map(declaredLine)} onPickStep={onPickStep} />
        </section>
      )}

      {tolerated.length > 0 && (
        <section className="verdict-section verdict-tolerated">
          <Head title="tolerated" count={tolerated.length} />
          <FindingList findings={tolerated.map(failureLine)} onPickStep={onPickStep} />
        </section>
      )}

      {abandoned !== undefined && (
        <section className="verdict-section verdict-abandoned">
          <h4>
            script ended <span className="count">{abandoned.pending.length} not run</span>{" "}
            <span className="muted">
              leg {abandoned.leg}
              {abandoned.step !== undefined && <> at <StepLink step={abandoned.step} onPickStep={onPickStep} /></>}
            </span>
          </h4>
          <FindingList
            findings={[
              ...(abandoned.pending.length === 0
                ? []
                : [{ rule: "not run", links: abandoned.pending, text: "" }]),
              ...abandoned.closed.map((act) => ({ rule: act.owed, party: `leg ${act.leg}`, text: act.sent }))
            ]}
            onPickStep={onPickStep}
          />
        </section>
      )}

      <FindingSection
        className="verdict-violations"
        title="rfc violations"
        fold
        findings={violations.map((v) => {
          const audit = auditLineOf(rfc, v)
          return {
            rule: v.rule,
            chip: v.gating
              ? { label: "gates", tone: "gating", title: "emitted by the SUT: gates" }
              : { label: "ref capture", tone: "party", title: "in the reference capture; not gating" },
            party: v.emitter,
            step: v.step,
            text: audit?.text ?? "",
            full: audit?.full ?? `${v.rule} at ${v.step} by ${v.emitter}`
          }
        })}
        onPickStep={onPickStep}
      />

      {failures.length > 0 && (
        <section className="verdict-section verdict-failures">
          <Head title="failures" count={failures.length} />
          <FindingList findings={failures.map(failureLine)} onPickStep={onPickStep} />
        </section>
      )}

      {informative.length > 0 && (
        <section className="verdict-section verdict-informative">
          <Head title="informative" count={informative.length} />
          <FindingList
            findings={informative.map((entry) => ({
              ...failureLine(entry.finding),
              chip: { label: entry.checkClass, tone: "advisory", title: "evaluated on a lane it does not gate on" }
            }))}
            onPickStep={onPickStep}
          />
        </section>
      )}
    </div>
  )
}
