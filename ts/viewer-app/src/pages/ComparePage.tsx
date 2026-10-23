/**
 * Two ladders side by side, with the alignment between them.
 *
 * Colour is the verdict: green where a scripted message was recorded, red
 * where it was not, amber where the run produced one nobody scripted.
 */
import { compareLayouts, type LadderEvent } from "@sip/viewer-core"
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { useAsync } from "../api.ts"
import { JsonView } from "../components/JsonView.tsx"
import { LadderPane } from "../components/LadderPane.tsx"
import { Link } from "../components/Link.tsx"
import { SipMessageView } from "../components/SipMessageView.tsx"
import { href, type SideRef } from "../router.ts"
import { loadSide, type SideData } from "../side.ts"

const label = (ref: SideRef): string => `${ref.kind} ${ref.id}`

export const ComparePage = ({ left, right }: { left: SideRef; right: SideRef }): JSX.Element => {
  const [selected, setSelected] = useState<LadderEvent | undefined>(undefined)
  const sides = useAsync(
    () => Promise.all([loadSide(left), loadSide(right)]),
    `${label(left)}|${label(right)}`
  )

  const comparison = useMemo(() => {
    if (sides.state !== "ok") return undefined
    const [a, b] = sides.value as [SideData, SideData]
    if (!a.layout || !b.layout) return undefined
    // The scenario is the side that states intent; if both are the same kind,
    // the left one plays that part.
    const scenarioFirst = a.origin === "scenario" || b.origin !== "scenario"
    const result = scenarioFirst
      ? compareLayouts(a.layout, b.layout)
      : compareLayouts(b.layout, a.layout)
    return {
      result,
      leftStatus: scenarioFirst ? result.scenarioStatus : result.recordingStatus,
      rightStatus: scenarioFirst ? result.recordingStatus : result.scenarioStatus
    }
  }, [sides])

  if (sides.state === "loading") return <p className="page muted">loading both sides…</p>
  if (sides.state === "error") return <p className="page error">{sides.message}</p>

  const [a, b] = sides.value as [SideData, SideData]

  return (
    <div className="page compare-page">
      <header className="page-head">
        <div>
          <h1>compare</h1>
          <p className="muted">{label(left)} vs {label(right)}</p>
        </div>
        {comparison && (
          <div className="page-actions counts">
            <span className="tag matched">{comparison.result.counts.matched} matched</span>
            <span className="tag missing">{comparison.result.counts.missing} missing</span>
            <span className="tag extra">{comparison.result.counts.extra} extra</span>
          </div>
        )}
      </header>

      <div className="compare-grid">
        {[a, b].map((side, index) => (
          <div key={side.ref.kind + side.ref.id}>
            {side.problem && <p className="error">{side.problem}</p>}
            {side.layout && (
              <LadderPane
                layout={side.layout}
                heading={`${side.ref.kind}: ${side.title}`}
                selectedId={selected?.id}
                onSelect={(event) => setSelected(event)}
                statusById={index === 0 ? comparison?.leftStatus : comparison?.rightStatus}
                legend={index === 0}
              />
            )}
          </div>
        ))}
      </div>

      <div className="split">
        <div className="split-main">
          {comparison && (
            <table className="listing">
              <thead>
                <tr>
                  <th>leg</th>
                  <th>verdict</th>
                  <th>message</th>
                  <th>scenario step</th>
                  <th>recorded</th>
                </tr>
              </thead>
              <tbody>
                {comparison.result.rows.map((row, index) => (
                  <tr key={`${row.leg}-${index}`} className={`row-${row.status}`}>
                    <td>{row.leg}</td>
                    <td>{row.status}</td>
                    <td>{row.label}</td>
                    <td>{row.scenarioEventId ?? "—"}</td>
                    <td>{row.recordingEventId ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
        <aside className="split-side">
          {selected
            ? (
              <>
                <h3>{selected.id}</h3>
                {selected.origin === "recording"
                  ? <SipMessageView record={selected.source} label="recorded message" />
                  : <JsonView value={selected.source} label="selected" />}
              </>
            )
            : <p className="muted">click a message on either side.</p>}
          <p className="muted">
            <Link to={href.index()}>back to the index</Link>
          </p>
        </aside>
      </div>
    </div>
  )
}
