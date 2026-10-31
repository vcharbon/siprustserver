/** One case's history as a drawer: its verdict per run and lane, what changed between runs, the flip table, the runs with their deltas. */
import type { JSX } from "react"
import type { Campaign } from "../api.ts"
import { resultOf } from "../campaign-facets.ts"
import { history } from "../campaign-history.ts"

const MARK: Readonly<Record<string, string>> = { pass: "✓", failure: "✗", error: "!", none: "—" }

export const CaseHistory = ({ view, caseId, lanes, onClose, onDelta }: {
  readonly view: Campaign
  readonly caseId: string
  readonly lanes: readonly string[]
  readonly onClose: () => void
  readonly onDelta: (run: number, lane: string) => void
}): JSX.Element => {
  const h = history(view, caseId, lanes)
  const known = view.cases.find((c) => c.caseId === caseId)
  const newest = [...view.runs]
  return (
    <aside className="cp-drawer" aria-label="case history">
      <button type="button" className="cp-close" onClick={onClose} title="close (Esc)">✕</button>
      <h2>Case history</h2>
      <p className="muted mono">{caseId}{known === undefined ? "" : ` · ${known.family}`}</p>
      <section className="cp-panel">
        <h3>Verdicts</h3>
        <table className="cp-hist">
          <thead>
            <tr><th />{h.runs.map((id) => <th key={id}>#{id}</th>)}</tr>
          </thead>
          <tbody>
            {lanes.map((lane) => (
              <tr key={lane}>
                <td className="mono">{lane}</td>
                {h.verdicts[lane]!.map((verdict, at) => (
                  <td key={at}>
                    <button
                      type="button"
                      className={`cp-dot ${verdict}`}
                      disabled={verdict === "none"}
                      onClick={() => onDelta(h.runs[at]!, lane)}
                      title={`#${h.runs[at]} ${verdict}`}
                    >
                      {MARK[verdict]}
                    </button>
                  </td>
                ))}
              </tr>
            ))}
            <tr>
              <td className="muted">changed</td>
              {h.changed.map((names, at) => <td key={at} className="muted mono cp-small">{names.join(" ")}</td>)}
            </tr>
          </tbody>
        </table>
      </section>
      <section className="cp-panel">
        <h3>Flips</h3>
        {h.flips.length === 0 ? <p className="muted">✓ every run</p> : (
          <table className="cp-hist">
            <thead>
              <tr><th />{h.runs.map((id) => <th key={id}>#{id}</th>)}</tr>
            </thead>
            <tbody>
              {h.flips.map((flip) => (
                <tr key={`${flip.lane}\u0000${flip.key}`}>
                  <td className="mono">{flip.lane} · {flip.key}</td>
                  {flip.marks.map((mark, at) => (
                    <td
                      key={at}
                      className={`${mark === "✗" ? "x" : mark === "✓" ? "v" : ""}${flip.changedAt.includes(h.runs[at]!) ? " chg" : ""}`}
                      title={flip.seen[at] ?? undefined}
                    >
                      {mark}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      <section className="cp-panel">
        <h3>Runs</h3>
        <table className="cp-table">
          <tbody>
            {newest.flatMap((run) =>
              lanes.map((lane) => {
                const result = resultOf(run, lane, caseId)
                if (result === undefined) return null
                const session = run.lanes.find((l) => l.lane === lane)?.session
                return (
                  <tr key={`${run.id}:${lane}`}>
                    <td className="mono">#{run.id}</td>
                    <td className="mono">{lane}</td>
                    <td><span className={`cp-dot ${result.outcome}`}>{MARK[result.outcome]}</span></td>
                    <td className="mono cp-small">
                      {result.checks.map((c) => <div key={`${c.anchor}${c.field}`}>✗ {c.anchor} · {c.field}</div>)}
                      {result.outcome === "error" && <div>{(result.detail ?? "").split("\n")[0]}</div>}
                    </td>
                    <td>
                      <button type="button" onClick={() => onDelta(run.id, lane)}>details</button>
                      <div className="muted mono cp-small">{session}/{result.cell}</div>
                    </td>
                  </tr>
                )
              })
            )}
          </tbody>
        </table>
      </section>
    </aside>
  )
}
