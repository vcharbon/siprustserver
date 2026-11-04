/** The run grid: groups × runs × lanes, each cell its row's worst verdict; a group opens to its cases, a case cell to its history. */
import type { JSX } from "react"
import { useState } from "react"
import type { Campaign } from "../api.ts"
import type { Grid, GridCell } from "../campaign-history.ts"

const tip = (label: string, cell: GridCell): string =>
  `${label} · ${cell.lane} · #${cell.run}: ${cell.worst === "none" ? "—" : `${cell.failing}/${cell.ran} ✗`}`

export const CampaignGrid = ({ view, grid, lanes, run, onCase }: {
  readonly view: Campaign
  readonly grid: Grid
  readonly lanes: readonly string[]
  readonly run: number
  readonly onCase: (caseId: string) => void
}): JSX.Element => {
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set())
  const toggle = (name: string) =>
    setOpen((now) => {
      const next = new Set(now)
      if (!next.delete(name)) next.add(name)
      return next
    })
  const titleOf = (caseId: string): string => {
    const known = view.cases.find((c) => c.caseId === caseId)
    const name = known?.title ?? caseId.split("/").pop() ?? caseId
    return known?.subcase === null || known?.subcase === undefined ? name : `${name} [${known.subcase}]`
  }
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
      <p className="muted cp-legend">
        <span className="cp-cell pass cp-key" /> ✓ pass · <span className="cp-cell failure cp-key" /> ✗ fail ·{" "}
        <span className="cp-cell error cp-key" /> ! error · <span className="cp-cell none cp-key" /> — not run ·{" "}
        <span className="cp-cell none cur cp-key" /> run read
      </p>
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
            <tr key={row.name} className="cp-family" onClick={() => toggle(row.name)}>
              <td className="cp-name">{open.has(row.name) ? "▾" : "▸"} {row.name} <span className="muted">{row.cases.length}</span></td>
              {cells(row.name, row.cells)}
            </tr>,
            ...(open.has(row.name)
              ? row.cases.map((c) => (
                <tr key={`${row.name}\u0000${c.caseId}`}>
                  <td className="cp-name cp-case" title={c.caseId}>{titleOf(c.caseId)}</td>
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
