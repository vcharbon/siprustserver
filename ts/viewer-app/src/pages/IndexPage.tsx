/** The index: every case and run the configured roots hold, plus a compare launcher. */
import { exclusionReasons, parseExclusions } from "@sip/viewer-core"
import type { JSX } from "react"
import { useState } from "react"
import { api, useAsync, type CaseSummary } from "../api.ts"
import { DropCaptures } from "../components/DropCaptures.tsx"
import { Link } from "../components/Link.tsx"
import { formatSideRef, href, navigate } from "../router.ts"

/**
 * Cases split by where they came from, because the two populations are managed
 * differently: an authored case is written and lives in git, a captured one is
 * regenerated from its pcap and is replaced wholesale.
 */
const ORIGINS: readonly { readonly origin: string; readonly title: string; readonly note: string }[] = [
  { origin: "authored", title: "authored", note: "written by hand and kept in git" },
  { origin: "capture", title: "captured", note: "generated from a pcap; regenerate rather than edit" }
]

const CaseTable = (
  { entries, rootLabel }: {
    entries: readonly CaseSummary[]
    rootLabel: (rootId: string) => string
  }
): JSX.Element => (
  <table className="listing">
    <thead>
      <tr>
        <th>id</th>
        <th>root</th>
        <th>family</th>
        <th>variant</th>
        <th>title</th>
        <th>lanes</th>
      </tr>
    </thead>
    <tbody>
      {entries.map((entry) => (
        <tr key={entry.id} className={`origin-${entry.origin}`}>
          <td><Link to={href.case(entry.id)}>{entry.caseId}</Link></td>
          <td className="muted">{rootLabel(entry.rootId)}</td>
          <td>{entry.family}</td>
          <td>{entry.variant}</td>
          <td>{entry.title}</td>
          <td className="muted">
            {Object.entries(entry.lanes).map(([lane, verdict]) => `${lane}:${verdict}`).join(" ")}
          </td>
        </tr>
      ))}
    </tbody>
  </table>
)

/**
 * The entries the pipeline refused to cut a case from. Their own table, because
 * the columns a case is read by — family, variant, lanes — say nothing about
 * one: what a reviewer needs is the reason token and the call it was charged on.
 */
const ExcludedTable = (
  { entries, rootLabel }: {
    entries: readonly CaseSummary[]
    rootLabel: (rootId: string) => string
  }
): JSX.Element => (
  <table className="listing excluded-listing">
    <thead>
      <tr>
        <th>id</th>
        <th>root</th>
        <th>reason</th>
        <th>calls</th>
      </tr>
    </thead>
    <tbody>
      {entries.map((entry) => {
        const read = parseExclusions(entry.excluded?.document)
        return (
          <tr key={entry.id} className="origin-excluded">
            <td><Link to={href.case(entry.id)}>{entry.caseId}</Link></td>
            <td className="muted">{rootLabel(entry.rootId)}</td>
            <td className="mono">
              {read.ok
                ? exclusionReasons(read.records).map((reason) => (
                  <span key={reason} className="exclusion-reason">{reason}</span>
                ))
                : read.reason}
            </td>
            <td className="muted mono break">
              {read.ok
                ? [...new Set(read.records.flatMap((record) => record.callIds))].join(", ")
                : ""}
            </td>
          </tr>
        )
      })}
    </tbody>
  </table>
)

export const IndexPage = (): JSX.Element => {
  const cases = useAsync(() => api.cases(), "cases")
  const runs = useAsync(() => api.runs(), "runs")
  const config = useAsync(() => api.config(), "config")
  const [left, setLeft] = useState("")
  const [right, setRight] = useState("")
  const rootLabel = (rootId: string): string =>
    config.state === "ok"
      ? config.value.caseRoots.find((root) => root.id === rootId)?.label ?? rootId
      : rootId
  // An excluded entry is listed, never hidden — but never mixed in with the
  // cases either: it has no ladder, no lanes, nothing to run and nothing to
  // compare, so it is absent from every affordance that acts on a case.
  const all = cases.state === "ok" ? cases.value : []
  const generated = all.filter((entry) => entry.excluded === undefined)
  const excluded = all.filter((entry) => entry.excluded !== undefined)

  return (
    <div className="page index-page">
      <h1>flow viewer</h1>

      {config.state === "ok" && config.value.triage !== undefined && (
        <DropCaptures lanes={[config.value.triage.lane]} />
      )}

      {cases.state === "loading" && <p className="muted">loading…</p>}
      {cases.state === "error" && <p className="error">{cases.message}</p>}
      {cases.state === "ok" && (
        <>
          {ORIGINS.map((group) => {
            const entries = generated.filter((entry) => entry.origin === group.origin)
            return (
              <section key={group.origin} className={`origin-section origin-${group.origin}`}>
                <h2>
                  <span className={`origin-badge origin-${group.origin}`}>{group.title}</span>
                  <span className="muted"> {entries.length} cases · {group.note}</span>
                </h2>
                {entries.length === 0
                  ? <p className="muted">none under the configured case roots</p>
                  : <CaseTable entries={entries} rootLabel={rootLabel} />}
              </section>
            )
          })}
          {(() => {
            const rest = generated.filter(
              (entry) => !ORIGINS.some((group) => group.origin === entry.origin)
            )
            return rest.length === 0 ? null : (
              <section className="origin-section origin-unknown">
                <h2>
                  <span className="origin-badge origin-unknown">origin not stated</span>
                  <span className="muted"> {rest.length} cases</span>
                </h2>
                <CaseTable entries={rest} rootLabel={rootLabel} />
              </section>
            )
          })()}
          {excluded.length > 0 && (
            <section className="origin-section origin-excluded">
              <h2>
                <span className="origin-badge origin-excluded">excluded</span>
                <span className="muted">
                  {" "}{excluded.length} entries · the pipeline refused to cut a case; the
                  evidence is kept and the capture is still readable
                </span>
              </h2>
              <ExcludedTable entries={excluded} rootLabel={rootLabel} />
            </section>
          )}
        </>
      )}

      <section>
        <h2>runs</h2>
        {runs.state === "ok" && runs.value.length === 0 && (
          <p className="muted">no run bundles under the configured run roots</p>
        )}
        {runs.state === "ok" && runs.value.length > 0 && (
          <table className="listing">
            <thead>
              <tr>
                <th>id</th>
                <th>legs</th>
                <th>verdict</th>
              </tr>
            </thead>
            <tbody>
              {runs.value.map((entry) => (
                <tr key={entry.id}>
                  <td><Link to={href.run(entry.id)}>{entry.id}</Link></td>
                  <td>{entry.legs.join(", ")}</td>
                  <td className="muted">{JSON.stringify(entry.verdict)?.slice(0, 90)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section>
        <h2>compare</h2>
        <div className="compare-launcher">
          <select value={left} onChange={(event) => setLeft(event.target.value)}>
            <option value="">left side…</option>
            {generated.map((entry) => (
              <option key={entry.id} value={formatSideRef({ kind: "case", id: entry.id })}>
                case {entry.id}
              </option>
            ))}
            {runs.state === "ok" &&
              runs.value.map((entry) => (
                <option key={entry.id} value={formatSideRef({ kind: "run", id: entry.id })}>
                  run {entry.id}
                </option>
              ))}
          </select>
          <select value={right} onChange={(event) => setRight(event.target.value)}>
            <option value="">right side…</option>
            {generated.map((entry) => (
              <option key={entry.id} value={formatSideRef({ kind: "case", id: entry.id })}>
                case {entry.id}
              </option>
            ))}
            {runs.state === "ok" &&
              runs.value.map((entry) => (
                <option key={entry.id} value={formatSideRef({ kind: "run", id: entry.id })}>
                  run {entry.id}
                </option>
              ))}
          </select>
          <button
            type="button"
            disabled={left === "" || right === ""}
            onClick={() => navigate(`/compare/${left}/${right}`)}
          >
            compare
          </button>
        </div>
      </section>

      {config.state === "ok" && (
        <section className="config-note">
          <h2>configuration</h2>
          <p className="muted">{config.value.configPath}</p>
          <ul className="muted">
            {config.value.caseRoots.map((root) => <li key={root.id}>case root {root.id}: {root.path}</li>)}
            {config.value.flowsRoots.map((root) => <li key={root.id}>flows root {root.id}: {root.path}</li>)}
            {config.value.runRoots.map((root) => <li key={root.id}>run root {root.id}: {root.path}</li>)}
          </ul>
        </section>
      )}
    </div>
  )
}
