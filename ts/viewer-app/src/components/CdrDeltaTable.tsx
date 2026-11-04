/**
 * The raw CDR lines of a cell beside its case's other sub-cases on its lane,
 * or beside its case on the run's other lanes: one row per field that
 * differs, one column per cell, the cell's own column marked. A `content`
 * row stands out, an `expected` row is shown in its own colour as an accepted
 * difference; `timing` and `identity` rows, expected to differ, are muted and
 * may be hidden.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type Campaign, type CdrDeltaKind, useAsync } from "../api.ts"
import { type DeltaAxis, deltaColumns } from "../campaign-cdr.ts"

const KIND: Readonly<Record<CdrDeltaKind, string>> = { timing: "◷", identity: "#", expected: "≈", content: "≠" }

const AXES: ReadonlyArray<{ readonly axis: DeltaAxis; readonly label: string }> = [
  { axis: "subcases", label: "sub-cases" },
  { axis: "lanes", label: "lanes" }
]

export const CdrDeltaTable = ({ view, caseId, run, lane }: {
  readonly view: Campaign
  readonly caseId: string
  readonly run: number
  readonly lane: string
}): JSX.Element | null => {
  const [asked, setAxis] = useState<DeltaAxis>("subcases")
  const [all, setAll] = useState(false)
  const offered = AXES.filter((a) => deltaColumns(view, caseId, run, lane, a.axis).length > 1)
  const axis = offered.some((a) => a.axis === asked) ? asked : offered[0]?.axis ?? asked
  const columns = deltaColumns(view, caseId, run, lane, axis)
  const key = columns.map((c) => `${c.session}/${c.cell}`).join(",")
  const read = useAsync(
    () => (columns.length > 1 ? api.campaignCdrDelta(view.capture, columns) : Promise.resolve(undefined)),
    `${view.capture}|${key}`
  )
  if (offered.length === 0) return null
  const own = columns.findIndex((c) => c.caseId === caseId && c.lane === lane)
  return (
    <section className="cp-panel">
      <h3>
        CDR delta{" "}
        <span className="cp-lanebuttons">
          {offered.map((a) => (
            <button key={a.axis} type="button" className={a.axis === axis ? "on" : ""} onClick={() => setAxis(a.axis)}>
              {a.label}
            </button>
          ))}
        </span>{" "}
        <label className="cp-check" title="list the timing and identity rows too">
          <input type="checkbox" checked={all} onChange={(event) => setAll(event.target.checked)} /> ◷ #
        </label>
      </h3>
      <p className="muted cp-small">≠ content · ≈ expected · ◷ timing · # identity</p>
      {read.state === "loading" && <p className="muted">loading…</p>}
      {read.state === "error" && <p className="cp-nok">CDR delta: {read.message}</p>}
      {read.state === "ok" && read.value !== undefined && (() => {
        const delta = read.value
        const count = (kind: CdrDeltaKind) => delta.rows.filter((row) => row.kind === kind).length
        const rows = delta.rows.filter((row) => all || row.kind === "content" || row.kind === "expected")
        return (
          <div className="cp-cdr-delta">
            <table className="cp-table">
              <thead>
                <tr>
                  <th />
                  <th className="cp-nowrap">call · line</th>
                  <th>field</th>
                  {columns.map((column, at) => (
                    <th key={at} className={at === own ? "cp-own" : ""}>
                      {column.label} <span className="muted">{delta.columns[at]?.lines ?? "—"}</span>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((row, at) => (
                  <tr key={at} className={`cp-delta-${row.kind}`}>
                    <td title={row.kind}>{KIND[row.kind]}</td>
                    <td className="mono cp-nowrap">{row.leg} · {row.line}{row.last > row.line ? `–${row.last}` : ""}</td>
                    <td className="mono">{row.field}</td>
                    {row.values.map((value, i) => (
                      <td key={i} className={`mono cp-expected${i === own ? " cp-own" : ""}`}>{value ?? "—"}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="muted cp-small">
              {count("content")} ≠ · {count("expected")} ≈ · {count("timing") + count("identity")} ◷ # · {delta.same} same
            </p>
          </div>
        )
      })()}
    </section>
  )
}
