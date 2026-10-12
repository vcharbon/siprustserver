/**
 * THE DELTA SHEET of a queue slice: the study page's per-message delta table,
 * taken over every call the queue's filter keeps rather than one cell.
 *
 * One line per (signature, class, rule) — every header per message per method
 * the replay put on the wire differently — with a few distinct samples, each a
 * link to the call and step it came from. The URL is the queue's own filter, so
 * the queue links here with what it is showing; the server reads every kept
 * call's cell off disk, which takes seconds on a whole lane.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, type DeltaLine, type DeltaSample, useAsync } from "../api.ts"
import { Link } from "../components/Link.tsx"
import { laneOf, rememberLane } from "../lane-memory.ts"
import { filterParams, readFilter } from "../queue-filter.ts"
import { href, navigate, useQuery } from "../router.ts"

/** The queue opens on the failures; so does a sheet linked without a scope. */
const DEFAULT_SCOPE = "failed"

const SCOPES = ["failed", "refused", "red", "all", "passed", "skipped"] as const

/** A compared side: a body row keeps its line structure, every other row lists its values. */
const side = (kind: string, values: readonly string[]): JSX.Element =>
  kind === "body"
    ? <pre className="mono body-text">{values.join("\n") || "—"}</pre>
    : <>{values.join(" | ") || "—"}</>

const Samples = (
  { line, lane }: { line: DeltaLine; lane: string }
): JSX.Element => (
  <table className="delta-samples">
    <tbody>
      {line.samples.map((sample: DeltaSample, index) => (
        <tr key={index}>
          <td className="mono">
            <Link
              to={href.triageStudy(sample.capture, {
                lane,
                case: sample.caseId,
                cell: `${sample.session}/${sample.cell}`,
                step: sample.step
              })}
            >
              {sample.step || "call"}
            </Link>
          </td>
          <td className="mono break">{side(line.kind, sample.captured)}</td>
          <td className="mono break">{side(line.kind, sample.replayed)}</td>
        </tr>
      ))}
    </tbody>
  </table>
)

export const DeltasPage = (): JSX.Element => {
  const query = useQuery()
  const lane = laneOf(query)
  useEffect(() => rememberLane(lane), [lane])
  // A scope the server does not know reads as the default there; the picker says so too.
  const asked = query.get("scope") ?? DEFAULT_SCOPE
  const scope = (SCOPES as ReadonlyArray<string>).includes(asked) ? asked : DEFAULT_SCOPE
  const q = query.get("q") ?? ""
  const filter = readFilter(query)
  const slice = { lane: lane === "" ? undefined : lane, scope, q, ...filterParams(filter) }
  const sheet = useAsync(() => api.deltaSheet(slice), `deltas?${query.toString()}#${lane}`)
  const lanes = useAsync(() => api.lanes(), "lanes")

  const [needle, setNeedle] = useState("")
  const [kind, setKind] = useState("")
  const [hidden, setHidden] = useState<ReadonlySet<string>>(new Set())

  /** Lane and scope are the server's question: a change is a new URL and a new read. */
  const move = (over: { lane?: string; scope?: string }): void =>
    navigate(href.deltas({ ...slice, ...over }))

  if (sheet.state === "loading") {
    return <p className="page muted">reading every kept call's delta sheet — a whole lane takes seconds…</p>
  }
  if (sheet.state === "error") return <p className="page error">{sheet.message}</p>
  const value = sheet.value
  if (!value.available) return <p className="page muted">not available — {value.detail}</p>

  const kinds = [...new Set(value.lines.map((line) => line.kind))].sort()
  const classes = [...new Set(value.lines.map((line) => line.klass))].sort()
  const words = needle.trim().toLowerCase()
  const kept = value.lines.filter((line) =>
    (kind === "" || line.kind === kind) &&
    !hidden.has(line.klass) &&
    (words === "" || line.signature.toLowerCase().includes(words) || line.rule.toLowerCase().includes(words))
  )
  const toggle = (klass: string): void =>
    setHidden((held) => {
      const next = new Set(held)
      if (next.has(klass)) next.delete(klass)
      else next.add(klass)
      return next
    })

  return (
    <div className="page deltas-page">
      <header className="page-head">
        <div>
          <h1>delta sheet</h1>
          <p className="muted">
            {value.matched} call(s) kept on lane <code>{value.lane}</code> in scope <code>{value.scope}</code>
            {q === "" ? null : <> matching <code>{q}</code></>} · {value.read} cell(s) read ·{" "}
            {value.unread} call(s) with no confrontation file (refused, skipped or bundle cleaned)
            {value.undecodable === 0 ? null : <> · {value.undecodable} file(s) that did not decode</>} · read in{" "}
            {(value.elapsedMs / 1000).toFixed(1)} s
          </p>
          {value.detail === "" ? null : <p className="error">{value.detail}</p>}
        </div>
      </header>

      <div className="queue-filters rules-filters">
        <label>
          lane
          <select value={value.lane} onChange={(event) => move({ lane: event.target.value })}>
            {[...new Set([value.lane, ...(lanes.state === "ok" ? lanes.value.lanes.map((one) => one.lane) : [])])].map((one) => (
              <option key={one} value={one}>{one}</option>
            ))}
          </select>
        </label>
        <label>
          calls
          <select value={scope} onChange={(event) => move({ scope: event.target.value })}>
            {SCOPES.map((one) => <option key={one} value={one}>{one}</option>)}
          </select>
        </label>
        <label>
          kind
          <select value={kind} onChange={(event) => setKind(event.target.value)}>
            <option value="">every kind</option>
            {kinds.map((one) => <option key={one} value={one}>{one}</option>)}
          </select>
        </label>
        <label>
          signature / rule
          <input value={needle} placeholder="contains…" onChange={(event) => setNeedle(event.target.value)} />
        </label>
        {classes.map((klass) => (
          <label key={klass}>
            <input type="checkbox" checked={!hidden.has(klass)} onChange={() => toggle(klass)} />
            <span className={`tag class-${klass}`}>{klass}</span>
          </label>
        ))}
        <span className="muted">{kept.length} of {value.lines.length} line(s)</span>
      </div>

      <table className="listing confrontation-listing deltas-listing">
        <thead>
          <tr>
            <th>kind</th>
            <th>signature</th>
            <th>class</th>
            <th>rule</th>
            <th>rows</th>
            <th>cells</th>
            <th>samples: step · captured · replayed</th>
          </tr>
        </thead>
        <tbody>
          {kept.map((line) => (
            <tr key={`${line.signature}\u0000${line.klass}\u0000${line.rule}`} className={`class-${line.klass}`}>
              <td>{line.kind}</td>
              <td className="mono break">{line.signature}</td>
              <td><span className={`tag class-${line.klass}`}>{line.klass}</span></td>
              <td className="mono break">{line.rule || "—"}</td>
              <td className="rule-count">{line.rows}</td>
              <td className="rule-count">{line.cells}</td>
              <td><Samples line={line} lane={value.lane} /></td>
            </tr>
          ))}
        </tbody>
      </table>

      <p className="muted">
        <Link to={href.triage({ ...slice, lane: value.lane })}>the triage queue of this slice</Link>
      </p>
    </div>
  )
}
