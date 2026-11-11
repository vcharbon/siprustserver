/**
 * A `records` attachment: the records side by side, a row per field in the
 * order fields first appear, a column per record (`#<n>`); a field a record
 * lacks is an empty cell. The table scrolls sideways in its box, the field
 * names staying in view.
 */
import type { JSX } from "react"

export const RecordsAttachment = ({ records }: {
  readonly records: ReadonlyArray<Readonly<Record<string, string>>>
}): JSX.Element => {
  if (records.length === 0) return <p className="muted">none</p>
  const fields = [...new Set(records.flatMap((record) => Object.keys(record)))]
  return (
    <div className="cp-side-scroll">
      <table className="cp-table at-cells cp-side">
        <thead>
          <tr>
            <th>field</th>
            {records.map((_, at) => <th key={at}>#{at + 1}</th>)}
          </tr>
        </thead>
        <tbody>
          {fields.map((field) => (
            <tr key={field}>
              <th scope="row" className="mono">{field}</th>
              {records.map((record, at) => <td key={at} className="mono">{record[field] ?? ""}</td>)}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
