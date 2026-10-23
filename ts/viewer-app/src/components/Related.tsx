/**
 * The case↔run cross-link, both directions of one join.
 *
 * A run bundle names the case it replayed in its `pivot.json` `case.id`, and a
 * case document carries that same id, so the pairing is that key on both sides
 * — never the entry name, which differs from it (`alt-answered.v3` holds
 * `alt-answered`). Several roots may hold the same case id; every match is
 * listed rather than one being guessed.
 */
import type { JSX } from "react"
import type { CaseSummary, RunSummary } from "../api.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/** The case definitions a run's `case.id` resolves to. */
export const CaseOfRun = (
  { caseKey, cases }: {
    caseKey: string | undefined
    cases: readonly CaseSummary[] | undefined
  }
): JSX.Element | null => {
  if (caseKey === undefined || cases === undefined) return null
  const matches = cases.filter((entry) => entry.caseKey === caseKey)
  return (
    <p className="related">
      <span className="related-label">case definition</span>
      {matches.length === 0
        ? (
          <span className="muted">
            none under the configured case roots for <code>{caseKey}</code>
          </span>
        )
        : matches.map((entry) => (
          <Link key={entry.id} className="related-link" to={href.case(entry.id)}>
            {entry.id}
          </Link>
        ))}
    </p>
  )
}

/** The run bundles that replayed a case. */
export const RunsOfCase = (
  { caseKey, runs }: {
    caseKey: string | undefined
    runs: readonly RunSummary[] | undefined
  }
): JSX.Element | null => {
  if (caseKey === undefined || runs === undefined) return null
  const matches = runs.filter((run) => run.caseKey === caseKey)
  if (matches.length === 0) return null
  return (
    <p className="related">
      <span className="related-label">run bundles</span>
      {matches.map((run) => (
        <Link key={run.id} className="related-link" to={href.run(run.id)}>
          {run.id}
        </Link>
      ))}
    </p>
  )
}
