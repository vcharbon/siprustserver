/**
 * The generator's own account of a case, on one line each: what its detectors
 * concluded, and what it could not decide.
 *
 * The DETECTOR ROSTER leads, because it is the part a sweep is read for — a
 * `detected-none:mrf` beside a `family: fork` is a claim a human refutes in
 * seconds, where silence is not readable at all. Everything else follows as the
 * diagnostics they are.
 */
import type { Annotations, Flag } from "@sip/viewer-core"

const ROSTER_FLAG = "detector-roster"

/** The three outcomes a rostered detector states, longest prefix first. */
const OUTCOMES = ["detection-unavailable", "detected-none", "detected"] as const

/**
 * The annotations of a document the API hands over untyped (a run bundle's
 * `pivot.json`), narrowed at the one place that reads them.
 */
export const annotationsOf = (pivot: unknown): Annotations | undefined => {
  const block = (pivot as { case?: { annotations?: unknown } } | null)?.case?.annotations
  return block === null || block === undefined ? undefined : (block as Annotations)
}

/** The outcome a flag states, or undefined for a plain diagnostic. */
const outcomeOf = (flag: Flag): (typeof OUTCOMES)[number] | undefined =>
  OUTCOMES.find((prefix) => flag.kind.startsWith(`${prefix}:`))

export const Flags = ({ annotations }: { annotations?: Annotations | null }) => {
  const flags = annotations?.flags ?? []
  if (flags.length === 0) return null
  const roster = flags.filter((f) => outcomeOf(f) !== undefined)
  const rest = flags.filter((f) => outcomeOf(f) === undefined && f.kind !== ROSTER_FLAG)

  return (
    <section className="case-flags">
      {roster.length > 0 && (
        <>
          <h3>detectors</h3>
          <ul className="flag-list">
            {roster.map((flag) => (
              <li key={flag.kind} className={`flag ${outcomeOf(flag)}`}>
                <code>{flag.kind}</code>
                <span className="flag-detail">{flag.detail}</span>
              </li>
            ))}
          </ul>
        </>
      )}
      {rest.length > 0 && (
        <>
          <h3>generator flags</h3>
          <ul className="flag-list">
            {rest.map((flag, i) => (
              <li key={`${flag.kind}-${i}`} className="flag">
                <code>{flag.kind}</code>
                <span className="flag-detail">{flag.detail}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  )
}
