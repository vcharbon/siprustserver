/**
 * THE RULES VIEW: every rule this deployment runs, and what the corpus came to
 * under it — one row per rule, ALWAYS, whether it fired ten thousand times or
 * not once.
 *
 * The page exists to find rules that are WRONG: too wide, never exercised, or
 * quietly doing nothing. So a zero is the loudest thing on it, and a zero is
 * always attributed — `declared` where the rule itself says nothing can reach
 * it, `never fired` where it could have and did not. A table of what fired
 * shows neither, which is how a rule that refuses nothing goes years unmeasured.
 *
 * Every count carries the population it was taken over, in the banner. A rule
 * reading zero over a population nothing measured is not a finding.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, useAsync, type RuleRow, type RulesView } from "../api.ts"
import { Link } from "../components/Link.tsx"
import { laneOf, rememberLane } from "../lane-memory.ts"
import { href, setQuery, useQuery } from "../router.ts"

/** The families, in the order a case meets them. */
const FAMILIES = [
  {
    id: "refusal",
    title: "refusal — deletes the case before any run",
    what: "hits = cases refused · reach = captures refused"
  },
  {
    id: "declaration",
    title: "declaration — states the failure the case OWES",
    what: "hits = must_fail entries · reach = case documents"
  },
  {
    id: "tolerance",
    title: "tolerance — names an accepted difference after the run",
    what: "hits = confrontation records named · reach = cells"
  },
  {
    id: "bless",
    title: "bless — restates a failed run as a pass",
    what: "hits = cells restated · reach = cells"
  }
] as const

/** The mark that opens a row: the defect first, then the blind spots. */
const markOf = (row: RuleRow): { readonly mark: string; readonly why: string } => {
  if ((row.offTier?.fired ?? 0) > 0) {
    return {
      mark: "!",
      why: `fires on ${row.offTier!.fired} cell(s) that PASSED — it refuses more than it must`
    }
  }
  if (row.standing === "unrostered") {
    return { mark: "#", why: "the corpus carries this token and no rule mints it" }
  }
  if (row.standing === "declared-zero") {
    return { mark: "=", why: row.inert ?? "the rule states that nothing can match it" }
  }
  if (row.standing === "unexercised") {
    return { mark: "0", why: "NEVER FIRED, and nothing declares that it could not" }
  }
  return { mark: "", why: "" }
}

/** The row's own reading, in the words its standing earns. */
const stateOf = (row: RuleRow, population: RulesView["population"]): string => {
  if (row.standing === "measured") {
    return `fired ${row.hits} time(s) over ${row.cells}`
  }
  if (row.standing === "declared-zero") return `DECLARED ZERO — ${row.inert ?? ""}`
  if (row.standing === "unrostered") return `UNROSTERED — ${row.hits} hit(s) nothing claims`
  const over = row.family === "refusal"
    ? `${population.refusalCaptures} capture(s)`
    : row.family === "declaration"
    ? `${population.cutCases} cut case(s)`
    : `${population.measuredRuns} measured run(s)`
  return `NEVER FIRED over ${over}`
}

/**
 * The subdivisions of a rule's count. `fired` is the headline itself and is not
 * restated; `shape` is a SUBSET of the classes beside it, and it is shown
 * because a rule that has never named a shape record can carry no bless.
 */
const Buckets = ({ row }: { row: RuleRow }): JSX.Element | null => {
  const shown = row.buckets.filter((bucket) => bucket.bucket !== "fired")
  if (shown.length === 0) return null
  return (
    <span className="rule-buckets">
      {shown.map((bucket) => (
        <span
          key={bucket.bucket}
          className={`tag class-${bucket.bucket}`}
          title={bucket.bucket === "shape"
            ? "shape records, whatever their class — the only kind a bless can cite"
            : `${bucket.cells} cell(s)`}
        >
          {bucket.bucket} {bucket.hits}
        </span>
      ))}
    </span>
  )
}

const Lanes = ({ row }: { row: RuleRow }): JSX.Element | null => {
  if (row.lanes.length === 0) return null
  return (
    <span className="rule-lanes">
      {row.lanes.map((lane) => (
        <span
          key={lane.lane}
          className={`tag class-${lane.class}`}
          title={lane.ticket === "" ? lane.lane : `${lane.lane} waits on ${lane.ticket}`}
        >
          {lane.lane}:{lane.class}
        </span>
      ))}
    </span>
  )
}

/**
 * The off-tier column: a refusal rule put to the cells that PASSED. A firing
 * here is the strongest "this rule is false" signal the system has, and an
 * absent pass says so rather than reading as a clean zero.
 */
const OffTier = ({ row, ran }: { row: RuleRow; ran: boolean }): JSX.Element => {
  // The pass evaluates REFUSAL predicates and nothing else: a post-run rule has
  // no green-cell reading to give, and an empty one must not read as a clean.
  if (row.family !== "refusal") return <span className="muted">n/a</span>
  if (!ran) return <span className="muted">no pass</span>
  if (row.offTier === undefined) return <span className="muted">not asked</span>
  if (row.offTier.fired > 0) {
    return (
      <span className="rule-fired" title={row.offTier.evidence.join(", ")}>
        FIRED on {row.offTier.fired} green
      </span>
    )
  }
  return (
    <span className="muted">
      clean on {row.offTier.silent}
      {row.offTier.unavailable === 0 ? "" : `, ${row.offTier.unavailable} unasked`}
    </span>
  )
}

const FamilyTable = (
  { family, rows, view }: {
    family: (typeof FAMILIES)[number]
    rows: readonly RuleRow[]
    view: RulesView
  }
): JSX.Element => {
  const zeros = rows.filter((row) => row.hits === 0).length
  const fired = rows.filter((row) => (row.offTier?.fired ?? 0) > 0).length
  return (
    <section className="rule-family">
      <h2>{family.title}</h2>
      <p className="muted">
        {rows.length} rule(s) · {zeros} never fired · {fired} firing on a cell that passed ·{" "}
        {family.what}
      </p>
      <table className="listing rule-listing">
        <thead>
          <tr>
            <th />
            <th>rule</th>
            <th>tier</th>
            <th>declares</th>
            <th>hits</th>
            <th>reach</th>
            <th>state</th>
            <th>off-tier</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const mark = markOf(row)
            return (
              <tr key={`${row.family}/${row.id}`} className={`rule-${row.standing}`}>
                <td className="rule-mark" title={mark.why}>{mark.mark}</td>
                <td className="rule-id">
                  {row.id}
                  {row.samples.length === 0 ? null : (
                    <div className="muted rule-samples" title={row.samples.join("\n")}>
                      e.g. {row.samples[0]}
                    </div>
                  )}
                </td>
                <td className="muted rule-tier">{row.stage} · {row.tier}</td>
                <td className="muted">
                  {row.subject === "" ? null : <span className="tag">{row.subject}</span>}
                  {row.disposition === "" ? null : <span className="tag">{row.disposition}</span>}
                  {row.unfalsifiable === undefined ? null : (
                    <span className="tag rule-unfalsifiable" title={row.unfalsifiable}>
                      no replay can falsify it
                    </span>
                  )}
                  <Lanes row={row} />
                </td>
                <td className="rule-count">{row.hits}</td>
                <td className="rule-count">{row.cells}</td>
                <td>
                  {stateOf(row, view.population)}
                  <Buckets row={row} />
                </td>
                <td><OffTier row={row} ran={view.offTierPass !== undefined} /></td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </section>
  )
}

/** What the counts were taken over, stated before any of them are read. */
const Population = ({ view }: { view: RulesView }): JSX.Element => (
  <p className="muted rule-population">
    refusals read from {view.population.refusalCaptures} capture(s) of the standing cut (
    {view.population.refusalRecords} record(s)) · post-run counts from{" "}
    {view.population.measuredRuns} of {view.population.standingRuns} standing run(s) on{" "}
    {view.lane} · declarations from {view.population.cutCases} case(s) over{" "}
    {view.population.cutCaptures} capture(s) ·{" "}
    {view.offTierPass === undefined
      ? "no cheap-falsifier pass has left a report"
      : `off-tier pass ${view.offTierPass.at.slice(0, 19)} over ${view.offTierPass.evaluated} green cell(s), ${view.offTierPass.findings} finding(s)`}
  </p>
)

export const RulesPage = (): JSX.Element => {
  const query = useQuery()
  const lane = laneOf(query)
  // The lane outlives the page: the next lane-less URL opens on it.
  useEffect(() => rememberLane(lane), [lane])
  const [needle, setNeedle] = useState(query.get("q") ?? "")
  const [zerosOnly, setZerosOnly] = useState(query.get("zeros") === "1")
  const view = useAsync(() => api.rules(lane === "" ? undefined : lane), `rules:${lane}`)

  if (view.state === "loading") return <p className="page muted">reading the roster…</p>
  if (view.state === "error") return <p className="page error">{view.message}</p>

  const kept = view.value.rows.filter((row) =>
    (needle.trim() === "" || row.id.toLowerCase().includes(needle.trim().toLowerCase())) &&
    (!zerosOnly || row.hits === 0 || (row.offTier?.fired ?? 0) > 0)
  )

  return (
    <div className="page rules-page">
      <header className="page-head">
        <div>
          <h1>rules</h1>
          <p className="muted">
            every rule of the roster, and what the corpus came to under it. A zero is a row,
            never an absence.
          </p>
          <Population view={view.value} />
          {view.value.detail === "" ? null : <p className="error">{view.value.detail}</p>}
        </div>
      </header>

      <div className="queue-filters rules-filters">
        <label>
          lane
          <select
            value={lane}
            onChange={(event) => {
              setQuery({ lane: event.target.value })
              window.dispatchEvent(new PopStateEvent("popstate"))
            }}
          >
            <option value="">{view.value.lane} (configured)</option>
            {view.value.lanes.map((entry) => (
              <option key={entry.lane} value={entry.lane}>
                {entry.lane} · {entry.captures} capture(s)
              </option>
            ))}
          </select>
        </label>
        <label>
          rule
          <input
            value={needle}
            placeholder="name contains…"
            onChange={(event) => setNeedle(event.target.value)}
          />
        </label>
        <label>
          <input
            type="checkbox"
            checked={zerosOnly}
            onChange={(event) => setZerosOnly(event.target.checked)}
          />
          suspicious only (never fired, or firing on a cell that passed)
        </label>
        <span className="muted">{kept.length} of {view.value.rows.length} rule(s)</span>
      </div>

      {FAMILIES.map((family) => (
        <FamilyTable
          key={family.id}
          family={family}
          rows={kept.filter((row) => row.family === family.id)}
          view={view.value}
        />
      ))}

      <p className="muted">
        <Link to={href.triage()}>the triage queue</Link> ·{" "}
        <Link to={href.index()}>the index</Link>
      </p>
    </div>
  )
}
