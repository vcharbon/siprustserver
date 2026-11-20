/**
 * A `table` attachment: its columns as the header, a row per row, every value
 * as text, a cell coloured by its class where the table states one
 * (`@sip/contracts` `Attachment.CellClass`).
 */
import type { Attachment } from "@sip/contracts"
import type { JSX } from "react"

/** The style of a cell of class `cls`. */
const CLASS_STYLE: Readonly<Record<Attachment.CellClass, string>> = { ok: "cp-ok", nok: "cp-nok", muted: "muted" }

const cellClass = (cls: Attachment.CellClass | null | undefined): string =>
  cls === null || cls === undefined ? "mono" : `mono ${CLASS_STYLE[cls]}`

export const TableAttachment = ({ columns, rows, classes }: Attachment.Table): JSX.Element =>
  rows.length === 0 ? <p className="muted">none</p> : (
    <div className="cp-side-scroll">
      <table className="cp-table at-cells">
        <thead>
          <tr>{columns.map((column, at) => <th key={at}>{column}</th>)}</tr>
        </thead>
        <tbody>
          {rows.map((row, at) => (
            <tr key={at}>{row.map((value, i) => <td key={i} className={cellClass(classes?.[at]?.[i])}>{value}</td>)}</tr>
          ))}
        </tbody>
      </table>
    </div>
  )
