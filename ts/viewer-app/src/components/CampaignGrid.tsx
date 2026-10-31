/** The run grid: families × runs × lanes, each cell its row's worst verdict; a family opens to its cases, a case cell to its history. */
import type { JSX } from "react"
import { useState } from "react"
import type { Grid, GridCell } from "../campaign-history.ts"

const tip = (label: string, cell: GridCell): string =>
  `${label} · ${cell.lane} · #${cell.run}: ${cell.worst === "none" ? "—" : `${cell.failing}/${cell.ran} ✗`}`

export const CampaignGrid = ({ grid, lanes, run, onCase }: {
  readonly grid: Grid
  readonly lanes: readonly string[]
  readonly run: number
  readonly onCase: (caseId: string) => void
}): JSX.Element => {
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set())
  const toggle = (family: string) =>
    setOpen((now) => {
      const next = new Set(now)
      if (!next.delete(family)) next.add(family)
      return next
    })
  const cells = (label: string, row: readonly GridCell[], caseId?: string) =>
    lanes.flatMap((lane) => [
      ...grid.runs.map((id) => {
        const cell = row.find((c) => c.lane === lane && c.run === id)!
        return (
          <td key={`${lane}:${id}`}>
            <span
              className={`cp-cell ${cell.worst}${id === run ? " cur" : ""}`}
              title={tip(label, cell)}
              onClick={caseId === undefined ? undefined : () => onCase(caseId)}
            />
          </td>
        )
      }),
      <td key={`${lane}:gap`} className="cp-gap" />
    ])
  return (
    <section className="cp-panel cp-grid">
      <table>
        <thead>
          <tr>
            <th />
            {lanes.flatMap((lane) => [
              <th key={lane} colSpan={grid.runs.length} className="mono">{lane}</th>,
              <th key={`${lane}:gap`} className="cp-gap" />
            ])}
          </tr>
          <tr>
            <th />
            {lanes.flatMap((lane) => [
              ...grid.runs.map((id) => <th key={`${lane}:${id}`} className="cp-run">{id}</th>),
              <th key={`${lane}:gap`} className="cp-gap" />
            ])}
          </tr>
        </thead>
        <tbody>
          {grid.rows.flatMap((row) => [
            <tr key={row.family} className="cp-family" onClick={() => toggle(row.family)}>
              <td className="cp-name">{open.has(row.family) ? "▾" : "▸"} {row.family} <span className="muted">{row.cases.length}</span></td>
              {cells(row.family, row.cells)}
            </tr>,
            ...(open.has(row.family)
              ? row.cases.map((c) => (
                <tr key={c.caseId}>
                  <td className="cp-name cp-case" title={c.caseId}>{c.caseId.split("/").pop()}</td>
                  {cells(c.caseId, c.cells, c.caseId)}
                </tr>
              ))
              : [])
          ])}
        </tbody>
      </table>
    </section>
  )
}
