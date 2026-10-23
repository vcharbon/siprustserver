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
  status === "ok-negative" ? "a negative case: it failed exactly as declared" : undefined

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
 * A failure's own tag, the step it was raised at, and the fields worth reading
 * before the raw JSON. The step is lifted out because it is an ADDRESS: it puts
 * the finding on the ladders, and reading it as prose loses that.
 */
const failureLine = (
  value: unknown
): { readonly tag: string; readonly step: string | undefined; readonly detail: string } => {
  if (!isRecord(value)) return { tag: "failure", step: undefined, detail: JSON.stringify(value) ?? "" }
  const tag = str(value.failure) ?? "failure"
  const step = str(value.step) ?? stepOfSite(value.site)
  const detail = Object.entries(value)
    .filter(([key]) => key !== "failure" && key !== "step" && !(key === "site" && step !== undefined))
    .map(([key, entry]) => `${key}: ${typeof entry === "string" ? entry : JSON.stringify(entry)}`)
    .join(" · ")
  return { tag, step, detail }
}

/** The step chip: the finding's address on every ladder that draws it. */
const StepLink = (
  { step, onPickStep }: { step: string; onPickStep: ((step: string) => void) | undefined }
): JSX.Element =>
  onPickStep === undefined
    ? <code className="verdict-step">{step}</code>
    : (
      <button
        type="button"
        className="step-link verdict-step"
        title={`show ${step} on the ladders`}
        onClick={() => onPickStep(step)}
      >
        {step}
      </button>
    )

const FindingLine = (
  { value, onPickStep }: { value: unknown; onPickStep?: ((step: string) => void) | undefined }
): JSX.Element => {
  const { tag, step, detail } = failureLine(value)
  return (
    <>
      <code className="verdict-tag">{tag}</code>
      {step !== undefined && <StepLink step={step} onPickStep={onPickStep} />}
      <span className="verdict-detail">{detail}</span>
    </>
  )
}

export const VerdictPane = (
  { verdict, onPickStep }: {
    verdict: unknown
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
          <h4>must fail ({declared.length})</h4>
          <p className="muted">
            the document declares the failure its run must produce, and passes only by producing
            exactly that one — any other failure, or none, and the run is red.
          </p>
          {declared.map((d) => (
            <div key={`${d.failure}-${d.step}`} className="verdict-row">
              <p className="verdict-row">
                <code className="verdict-tag">{d.failure}</code>
                <span className="verdict-detail">
                  anchor <StepLink step={d.step} onPickStep={onPickStep} /> · derived from{" "}
                  {d.derivedFrom}
                </span>
              </p>
              <p className="verdict-row verdict-declared-outcome">
                {d.observed !== undefined ? (
                  <FindingLine value={d.observed} onPickStep={onPickStep} />
                ) : d.recorded !== undefined ? (
                  <span className="verdict-detail">
                    recorded after the script ended, with no expect armed to refuse it:{" "}
                    {d.recorded}
                  </span>
                ) : (
                  <span className="verdict-declared-absent">the run did not produce it</span>
                )}
              </p>
            </div>
          ))}
        </section>
      )}

      {tolerated.length > 0 && (
        <section className="verdict-section verdict-tolerated">
          <h4>tolerated ({tolerated.length})</h4>
          <p className="muted">
            wire divergences at or after the declared one. A negative case replays a capture it is
            known to diverge from, so these are carried — and shown, never dropped. A settle, CDR or
            postcondition failure would still be red.
          </p>
          {tolerated.map((failure, index) => (
            // The list is the run's own order and nothing else identifies a row.
            <p key={index} className="verdict-row">
              <FindingLine value={failure} onPickStep={onPickStep} />
            </p>
          ))}
        </section>
      )}

      {abandoned !== undefined && (
        <section className="verdict-section verdict-abandoned">
          <h4>script ended ({abandoned.pending.length} step(s) not run)</h4>
          <p className="muted">
            the run could not go on, so the SCRIPT ended — not the call. Leg {abandoned.leg}
            {abandoned.step !== undefined && (
              <> at <StepLink step={abandoned.step} onPickStep={onPickStep} /></>
            )} — and the scripted ends
            then closed what they held, so the run still settled and still billed.
          </p>
          {abandoned.pending.length > 0 && (
            <p className="verdict-row">
              <code className="verdict-tag">not run</code>
              <span className="verdict-detail">
                {abandoned.pending.map((pending) => (
                  <StepLink key={pending} step={pending} onPickStep={onPickStep} />
                ))}
              </span>
            </p>
          )}
          {abandoned.closed.map((act, index) => (
            // The list is the close's own order and nothing else identifies a row.
            <p key={index} className="verdict-row">
              <code className="verdict-tag">{act.owed}</code>
              <span className="verdict-detail">
                leg {act.leg} · {act.sent}
              </span>
            </p>
          ))}
        </section>
      )}

      {violations.length > 0 && (
        <section className="verdict-section verdict-violations">
          <h4>rfc violations ({violations.length})</h4>
          {violations.map((v) => (
            <p key={`${v.rule}-${v.step}`} className="verdict-row">
              <code className="verdict-tag">{v.rule}</code>
              <span className="verdict-detail">
                step <StepLink step={v.step} onPickStep={onPickStep} /> · emitted by {v.emitter} ·{" "}
                {v.gating ? "the system under test: it GATES" : "a scripted peer: listed, gates nothing"}
              </span>
            </p>
          ))}
        </section>
      )}

      {failures.length > 0 && (
        <section className="verdict-section verdict-failures">
          <h4>failures ({failures.length})</h4>
          {failures.map((failure, index) => (
            // The list is the run's own order and nothing else identifies a row.
            <p key={index} className="verdict-row">
              <FindingLine value={failure} onPickStep={onPickStep} />
            </p>
          ))}
        </section>
      )}

      {informative.length > 0 && (
        <section className="verdict-section verdict-informative">
          <h4>informative ({informative.length})</h4>
          <p className="muted">
            evaluated on a lane they do not gate on — the origin platform's vocabulary, kept as a
            fact rather than dropped.
          </p>
          {informative.map((note, index) => (
            <p key={index} className="verdict-row">
              <code className="verdict-class">{note.checkClass}</code>
              <FindingLine value={note.finding} onPickStep={onPickStep} />
            </p>
          ))}
        </section>
      )}
    </div>
  )
}
