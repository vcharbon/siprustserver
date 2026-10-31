/** The failures of the run read as facets: checks and families to pick, the calls they narrow to, one check's shapes. */
import type { JSX } from "react"
import type { Campaign } from "../api.ts"
import type { Facets } from "../campaign-facets.ts"
import { CheckCard, SideTag } from "./CheckCard.tsx"

const MARK: Readonly<Record<string, string>> = { pass: "✓", failure: "✗", error: "!", none: "—" }

export const CampaignFailures = ({ view, facets, run, vs, lanes, selected, onToggle, onCase }: {
  readonly view: Campaign
  readonly facets: Facets
  readonly run: number
  readonly vs: number | undefined
  readonly lanes: readonly string[]
  readonly selected: { readonly checks: readonly string[]; readonly families: readonly string[] }
  readonly onToggle: (kind: "check" | "family", key: string) => void
  readonly onCase: (caseId: string) => void
}): JSX.Element => {
  const single = selected.checks.length === 1 ? selected.checks[0] : undefined
  return (
    <div className="cp-facets">
      <aside>
        <section className="cp-panel">
          <h2>Failed checks</h2>
          {facets.checks.length === 0 && <p className="muted">none</p>}
          {facets.checks.map((f) => (
            <button
              key={f.key}
              type="button"
              className={`cp-facet${selected.checks.includes(f.key) ? " on" : ""}`}
              onClick={() => onToggle("check", f.key)}
            >
              <span className="cp-count">{f.calls}</span>
              <span className="mono">{f.anchor} · {f.field}</span>
              <span className="cp-tags">
                {Object.entries(f.sides).map(([side, n]) => (
                  <SideTag key={side} side={side} sides={view.sides} count={Object.keys(f.sides).length > 1 ? n : undefined} />
                ))}
                {f.since === run ? <span className="cp-tag new">new #{run}</span> : <span className="cp-tag">since #{f.since}</span>}
              </span>
              <span className="cp-lanes mono">
                {Object.entries(f.lanes).map(([lane, n]) => `${lane} ${n}`).join(" · ")}
              </span>
            </button>
          ))}
          {facets.passingAgain.map((f) => (
            <div key={f.key} className="cp-facet gone" title={`failed in #${vs}`}>
              <span className="cp-count">{f.calls}</span>
              <span className="mono">{f.key}</span>
              <span className="cp-tags"><span className="cp-tag fixed">✓ since #{vs}</span></span>
            </div>
          ))}
        </section>
        <section className="cp-panel">
          <h2>Families</h2>
          <div className="cp-chips">
            {facets.families.map((f) => (
              <button
                key={f.family}
                type="button"
                className={`cp-chip${selected.families.includes(f.family) ? " on" : ""}`}
                onClick={() => onToggle("family", f.family)}
              >
                {f.family} <b>{f.calls}</b>
              </button>
            ))}
            {facets.families.length === 0 && <span className="muted">none</span>}
          </div>
        </section>
      </aside>
      <section className="cp-panel">
        <div className="cp-matched">
          <b>{facets.matching.length}</b> failing of {facets.ran}
          {[...selected.checks.map((k) => ["check", k] as const), ...selected.families.map((k) => ["family", k] as const)].map(
            ([kind, key]) => (
              <button key={`${kind}:${key}`} type="button" className="cp-chip on" onClick={() => onToggle(kind, key)}>
                {key} ✕
              </button>
            )
          )}
        </div>
        {single !== undefined && facets.shapes.map((shape) => (
          <CheckCard
            key={`${shape.shape}\u0000${shape.side}`}
            check={shape.sample}
            sides={view.sides}
            saidLabel={view.saidLabel}
            head={<span className="mono">{shape.count} call·lane · {single}</span>}
          />
        ))}
        <table className="cp-table">
          <thead>
            <tr>
              <th>case</th>
              {lanes.map((lane) => <th key={lane} className="mono">{lane}</th>)}
              <th>failed checks</th>
            </tr>
          </thead>
          <tbody>
            {facets.matching.map((m) => (
              <tr key={m.caseId} className="cp-row" onClick={() => onCase(m.caseId)}>
                <td>
                  {m.caseId}
                  <div className="muted mono">{m.family}</div>
                </td>
                {lanes.map((lane) => (
                  <td key={lane}><span className={`cp-dot ${m.outcomes[lane]}`} title={m.outcomes[lane]}>{MARK[m.outcomes[lane] ?? "none"]}</span></td>
                ))}
                <td className="mono">
                  {m.failed.map((f) => (
                    <div key={f.key} className={selected.checks.includes(f.key) ? "cp-nok" : ""}>
                      ✗ {f.key} <span className="muted">{f.lanes.join(",")}</span>
                    </div>
                  ))}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  )
}
