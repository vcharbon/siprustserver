/**
 * The `records` attachments of one name compared between cells (ADR-0060
 * appendix §9): records align by position, each a group `#<n>`; a row is a
 * field whose values are not all the same among the columns holding the
 * record, every column's value shown (null where it holds no such record). A
 * record a column lacks while holding the attachment is one `record` row,
 * `present` against `absent`; a column holding no attachment (null) stands
 * out of every comparison. Fields come in the order they first appear.
 */
import type { Attachment } from "@sip/contracts"

type FlatRecord = Readonly<Record<string, string>>

/** The diff of `sides`, each a column's records (null where it holds none), headed by `columns`. */
export const recordsDiff = (
  columns: ReadonlyArray<string>,
  sides: ReadonlyArray<ReadonlyArray<FlatRecord> | null>
): Attachment.Diff => {
  const rows: Array<Attachment.DiffRow> = []
  let same = 0
  const depth = Math.max(0, ...sides.map((side) => side?.length ?? 0))
  for (let at = 0; at < depth; at++) {
    const group = `#${at + 1}`
    const records = sides.map((side) => side?.[at])
    if (records.some((record, i) => record === undefined && sides[i] !== null)) {
      rows.push({
        group,
        field: "record",
        values: records.map((record, i) => (sides[i] === null ? null : record === undefined ? "absent" : "present"))
      })
    }
    const held = records.filter((record): record is FlatRecord => record !== undefined)
    if (held.length < 2) continue
    const fields = [...new Set(held.flatMap((record) => Object.keys(record)))]
    for (const field of fields) {
      if (new Set(held.map((record) => record[field] ?? "")).size <= 1) {
        same++
        continue
      }
      rows.push({ group, field, values: records.map((record) => (record === undefined ? null : record[field] ?? "")) })
    }
  }
  return { columns, rows, same }
}
