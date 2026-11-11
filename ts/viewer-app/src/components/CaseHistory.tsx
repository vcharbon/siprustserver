/**
 * One case's history as a drawer, partial runs among the full ones (`°`), with
 * a replay of the case on its lanes: what the case is (its title, groups,
 * description, what it uses, each linked to its page where there is one, and
 * the file it was compiled from, folded), every check it makes marked per
 * lane (folded),
 * its verdict per run and lane, what changed between runs, the flip table, the
 * runs with their status, waived lines and deltas and, for a cell replayed more than once, its attempts,
 * over the latest runs the case ran in unless every one is asked for.
 */
import { type JSX, useState } from "react"
import type { Campaign } from "../api.ts"
import { resultOf, statusOf } from "../campaign-facets.ts"
import { attemptLines } from "../campaign-attempts.ts"
import { checkList } from "../campaign-checks.ts"
import { exclusionsOf } from "../campaign-tags.ts"
import { CaseReplay } from "./CampaignLaunch.tsx"
import { history } from "../campaign-history.ts"
import { sourceLines } from "../source-lines.ts"

/** How many of the runs the case ran in the drawer shows before "show all". */
const LATEST = 12

const MARK: Readonly<Record<string, string>> = { pass: "✓", failure: "✗", error: "!", none: "—" }

/** A run's column head: its id, a partial run's marked with `°` and its scope as the tooltip. */
const RunHead = ({ id, scope }: { readonly id: number; readonly scope: string | null }): JSX.Element => (
  <th title={scope === null ? undefined : `partial: ${scope}`}>#{id}{scope === null ? "" : "°"}</th>
)

/** How an operator reads before its expected value. */
const OP: Readonly<Record<string, string>> = { eq: "=", regex: "~", absent: "absent", exists: "present" }

export const CaseHistory = ({ view, caseId, lanes, onClose, onDelta }: {
  readonly view: Campaign
  readonly caseId: string
  readonly lanes: readonly string[]
  readonly onClose: () => void
  readonly onDelta: (run: number, lane: string) => void
}): JSX.Element => {
  const [all, setAll] = useState(false)
  const h = history(view, caseId, lanes, all ? Number.POSITIVE_INFINITY : LATEST)
  const checks = checkList(view, caseId, lanes)
  const scopeOf = (id: number): string | null => view.runs.find((r) => r.id === id)?.scope ?? null
  const known = view.cases.find((c) => c.caseId === caseId)
  const newest = [...view.runs].filter((run) => h.runs.includes(run.id)).sort((a, b) => b.id - a.id)
  return (
    <aside className="cp-drawer" aria-label="case history">
      <button type="button" className="cp-close" onClick={onClose} title="close (Esc)">✕</button>
      <h2>{known?.title ?? "Case history"}</h2>
      <p className="muted mono">{caseId}{known === undefined ? "" : ` · ${known.family}`}</p>
      {known !== undefined && view.launch !== null && (
        <CaseReplay
          capture={view.capture}
          lanes={view.launch.lanes}
          scope={{ case: known.ref, ...(known.subcase === null ? {} : { subcase: known.subcase }) }}
          tags={view.launch.tags}
          carried={known.tags}
        />
      )}
      {exclusionsOf(view.runs, caseId).map((one) => (
        <p key={one.lane} className="muted mono">{one.lane} · {one.reason}</p>
      ))}
      {known !== undefined && known.groups.length > 0 && (
        <div className="cp-chips">{known.groups.map((group) => <span key={group} className="cp-chip">{group}</span>)}</div>
      )}
      {known?.description !== null && known?.description !== undefined && <p>{known.description}</p>}
      {known !== undefined && known.links.length > 0 && (
        <section className="cp-panel">
          <h3>Uses</h3>
          <table className="cp-table cp-uses">
            <tbody>
              {known.links.map((link) => (
                <tr key={`${link.role}\u0000${link.label}`}>
                  <td className="muted">{link.role}</td>
                  <td className="mono">
                    {link.url === null ? link.label : (
                      <a href={link.url} target="_blank" rel="noreferrer">{link.label} ↗</a>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
      {known?.source !== null && known?.source !== undefined && (
        <section className="cp-panel">
          <details>
            <summary><b>Source</b> <span className="muted mono">{known.source.file}</span></summary>
            <pre className="cp-source mono">
              {sourceLines(known.source.text).map((line, at) => (
                <span key={at} className="cp-line">{line}{"\n"}</span>
              ))}
            </pre>
          </details>
        </section>
      )}
      <section className="cp-panel">
        <h3>
          Verdicts{" "}
          {(h.hidden > 0 || all) && (
            <span className="muted cp-small">
              {all ? `all ${h.runs.length} runs` : `latest ${h.runs.length} of ${h.runs.length + h.hidden} runs`}{" "}
              <button type="button" onClick={() => setAll(!all)}>{all ? `latest ${LATEST}` : "show all"}</button>
            </span>
          )}
        </h3>
        <table className="cp-hist">
          <thead>
            <tr><th />{h.runs.map((id) => <RunHead key={id} id={id} scope={scopeOf(id)} />)}</tr>
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
        {h.runs.length === 0 ? <p className="muted">none</p> : h.flips.length === 0 ? <p className="muted">✓ every run</p> : (
          <table className="cp-hist">
            <thead>
              <tr><th />{h.runs.map((id) => <RunHead key={id} id={id} scope={scopeOf(id)} />)}</tr>
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
      {checks.rows.length > 0 && (
        <section className="cp-panel">
          <details>
            <summary>
              <b>Checks</b> <span className="muted">{checks.rows.length}{checks.run === undefined ? "" : ` · #${checks.run}`}</span>
            </summary>
            <table className="cp-table cp-checks">
              <thead>
                <tr>
                  <th>where</th>
                  <th>check</th>
                  {lanes.map((lane) => <th key={lane} className="mono">{lane}</th>)}
                </tr>
              </thead>
              <tbody>
                {checks.rows.map((row, at) => (
                  <tr key={at}>
                    <td className="mono">{row.anchor}</td>
                    <td className="mono">
                      {row.field === "*" ? "" : row.field} {OP[row.op] ?? row.op}{" "}
                      {row.expected === null ? null : <span className="cp-expected">{row.expected}</span>}
                    </td>
                    {lanes.map((lane) => (
                      <td key={lane}>
                        <span className={`cp-dot ${row.marks[lane]}`}>{MARK[row.marks[lane] ?? "none"]}</span>
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </details>
        </section>
      )}
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
                    <td>
                      <span className={`cp-dot ${result.outcome}`}>{MARK[result.outcome]}</span>
                      <div className="muted cp-small">{statusOf(result)}</div>
                    </td>
                    <td className="mono cp-small">
                      {result.checks.map((c) => <div key={`${c.anchor}${c.field}`}>✗ {c.anchor} · {c.field}</div>)}
                      {(result.lines ?? []).filter((line) => line.waived).map((line, at) => (
                        <div key={`waived${at}`} className="muted">waived {line.name}{line.message === null ? "" : ` · ${line.message}`}</div>
                      ))}
                      {result.outcome === "error" && <div>{(result.detail ?? "").split("\n")[0]}</div>}
                    </td>
                    <td>
                      <button type="button" onClick={() => onDelta(run.id, lane)}>details</button>
                      <div className="muted mono cp-small">{session}/{result.cell}</div>
                      {attemptLines(result).map((line) => <div key={line} className="muted mono cp-small">{line}</div>)}
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
