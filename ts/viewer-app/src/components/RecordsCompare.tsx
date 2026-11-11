/**
 * A campaign cell's `records` attachments compared with the attachment of
 * the same name of its case's other sub-cases on its lane, or of its case on
 * the run's other lanes (`campaign-columns.ts`), field by field
 * (`records-diff.ts`), the cell's own column marked. A cell holding no
 * `records` attachment, or with no other cell on either axis, shows nothing;
 * a cell holding several picks one by name.
 */
import type { Attachment } from "@sip/contracts"
import type { JSX } from "react"
import { useState } from "react"
import { api, type AttachmentListed, type Campaign, useAsync } from "../api.ts"
import { type CompareAxis, type CompareColumn, compareColumns } from "../campaign-columns.ts"
import { recordsDiff } from "../records-diff.ts"
import { DiffAttachment } from "./attachments/DiffAttachment.tsx"

const AXES: ReadonlyArray<{ readonly axis: CompareAxis; readonly label: string }> = [
  { axis: "subcases", label: "sub-cases" },
  { axis: "lanes", label: "lanes" }
]

/** The records attachment `name` of each of `columns`, null where its cell holds none. */
const recordsOf = (columns: readonly CompareColumn[], name: string) =>
  Promise.all(columns.map(async (column) => {
    const listed = await api.attachments({ run: column.run, case: column.caseId })
    const one = listed.find((held) => held.name === name && held.kind === "records")
    if (one === undefined) return null
    const view = await api.attachment(one.id)
    return view.kind === "records" ? view.records : null
  }))

const Compared = ({ columns, name, own }: {
  readonly columns: readonly CompareColumn[]
  readonly name: string
  readonly own: number
}): JSX.Element => {
  const key = `${name}|${columns.map((c) => `${c.run}/${c.caseId}`).join(",")}`
  const read = useAsync(async (): Promise<Attachment.Diff> => {
    const sides = await recordsOf(columns, name)
    return recordsDiff(columns.map((column, at) => `${column.label} ${sides[at]?.length ?? "—"}`), sides)
  }, key)
  if (read.state === "loading") return <p className="muted">loading…</p>
  if (read.state === "error") return <p className="cp-nok">compare: {read.message}</p>
  return <DiffAttachment diff={read.value} own={own} />
}

export const RecordsCompare = ({ view, caseId, run, lane, listed }: {
  readonly view: Campaign
  readonly caseId: string
  readonly run: number
  readonly lane: string
  readonly listed: readonly AttachmentListed[]
}): JSX.Element | null => {
  const names = listed.filter((one) => one.kind === "records").map((one) => one.name)
  const [askedAxis, setAxis] = useState<CompareAxis>("subcases")
  const [askedName, setName] = useState<string | undefined>(undefined)
  const offered = AXES.filter((a) => compareColumns(view, caseId, run, lane, a.axis).length > 1)
  if (names.length === 0 || offered.length === 0) return null
  const axis = offered.some((a) => a.axis === askedAxis) ? askedAxis : offered[0]!.axis
  const name = askedName !== undefined && names.includes(askedName) ? askedName : names[0]!
  const columns = compareColumns(view, caseId, run, lane, axis)
  const own = columns.findIndex((c) => c.caseId === caseId && c.lane === lane)
  return (
    <section className="cp-panel">
      <h3>
        Compare{" "}
        <span className="cp-lanebuttons">
          {offered.map((a) => (
            <button key={a.axis} type="button" className={a.axis === axis ? "on" : ""} onClick={() => setAxis(a.axis)}>
              {a.label}
            </button>
          ))}
        </span>{" "}
        {names.length > 1
          ? (
            <select value={name} onChange={(event) => setName(event.target.value)} aria-label="records compared">
              {names.map((one) => <option key={one} value={one}>{one}</option>)}
            </select>
          )
          : <span className="muted cp-small">{name}</span>}
      </h3>
      <Compared columns={columns} name={name} own={own} />
    </section>
  )
}
