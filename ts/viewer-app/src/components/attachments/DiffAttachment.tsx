/**
 * A `diff` attachment, or a comparison drawn the same way: a row per field
 * that differs (its group, its field, its class where it states one), a
 * column per side, `own` marked where given. A row of a class the diff calls
 * quiet is muted and listed only once asked; the count of each kind of row
 * and of the fields alike closes the table.
 */
import type { Attachment } from "@sip/contracts"
import type { JSX } from "react"
import { useState } from "react"

/** How many rows of each of `classes` (of every row where it states none), then how many fields alike. */
const counts = (diff: Attachment.Diff, classes: readonly string[]): readonly string[] => [
  ...(classes.length === 0
    ? [`${diff.rows.length} differ`]
    : classes.map((one) => `${diff.rows.filter((row) => row.class === one).length} ${one}`)),
  ...(diff.same === undefined ? [] : [`${diff.same} same`])
]

export const DiffAttachment = ({ diff, own }: {
  readonly diff: Attachment.Diff
  readonly own?: number | undefined
}): JSX.Element => {
  const [all, setAll] = useState(false)
  const quiet = new Set(diff.quiet ?? [])
  const isQuiet = (row: Attachment.DiffRow): boolean => row.class !== undefined && quiet.has(row.class)
  const hidden = diff.rows.filter(isQuiet).length
  const rows = all ? diff.rows : diff.rows.filter((row) => !isQuiet(row))
  const classes = [...new Set(diff.rows.flatMap((row) => (row.class === undefined ? [] : [row.class])))]
  const marked = (at: number): string => (at === own ? " cp-own" : "")
  return (
    <div className="at-diff">
      {hidden > 0 && (
        <label className="cp-check" title={`list the ${[...quiet].join(", ")} rows too`}>
          <input type="checkbox" checked={all} onChange={(event) => setAll(event.target.checked)} /> {[...quiet].join(" · ")}
        </label>
      )}
      {rows.length === 0 ? <p className="muted">none</p> : (
        <table className="cp-table at-cells">
          <thead>
            <tr>
              {classes.length > 0 && <th />}
              <th className="cp-nowrap">record</th>
              <th>field</th>
              {diff.columns.map((column, at) => <th key={at} className={marked(at).trim()}>{column}</th>)}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, at) => (
              <tr key={at} className={isQuiet(row) ? "at-quiet" : row.class === undefined ? "" : "at-loud"}>
                {classes.length > 0 && <td className="muted">{row.class ?? ""}</td>}
                <td className="mono cp-nowrap">{row.group ?? ""}</td>
                <td className="mono">{row.field}</td>
                {row.values.map((value, i) => (
                  <td key={i} className={`mono cp-expected${marked(i)}`}>{value ?? "—"}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <p className="muted cp-small">{counts(diff, classes).join(" · ")}</p>
    </div>
  )
}
