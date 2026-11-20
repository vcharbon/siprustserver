/**
 * The **attachment** (ADR-0060 appendix §9): a file a cell, a batch or a job
 * holds, typed by its kind so the site renders every file of a kind with one
 * component and names no file itself.
 *
 * A holder lists its attachments in its directory's {@link FILE}, one JSON
 * line each, every writer appending its own ({@link line}); the last line
 * naming an attachment states it ({@link parse}). `file` is relative to the
 * holder's directory, and is the URL itself for a `link`.
 *
 * What each kind's file holds:
 * - `log`: text, or JSON lines;
 * - `records`: JSON lines, one flat object per record;
 * - `diff`: one {@link Diff} document;
 * - `table`: JSON lines, the first the column names, each other one row, a
 *   cell its text or `{"value", "class"}` ({@link CellClass});
 * - `json`: one JSON document;
 * - `file`: anything, served as it is;
 * - `link`: nothing, `file` is a URL.
 */
import * as Schema from "effect/Schema"
import { KINDS } from "./attachment-kinds.js"

export { KINDS } from "./attachment-kinds.js"

export const Kind = Schema.Literals(KINDS)
export type Kind = typeof Kind.Type

export const Attachment = Schema.Struct({
  /** What it is, unique within its holder: a later attachment of the name replaces it. */
  name: Schema.String,
  kind: Kind,
  /** What the site heads it with; its name where absent. */
  title: Schema.optionalKey(Schema.String),
  /** Relative to the holder's directory; the URL of a `link`. */
  file: Schema.String,
  /** Who attached it: the replay, a collector, a judge. */
  source: Schema.String
})
export interface Attachment extends Schema.Schema.Type<typeof Attachment> {}

/** The file a holder's directory lists its attachments in. */
export const FILE = "attachments.jsonl"

const decodeAttachment = Schema.decodeUnknownOption(Attachment)

/** `attachment` as the line {@link FILE} holds it, newline included. */
export const line = (attachment: Attachment): string => `${JSON.stringify(attachment)}\n`

/**
 * The attachments a {@link FILE} text lists, in the order each name first
 * comes, each as its last line states it. A line that is no attachment, or a
 * file a `..` segment or an absolute path would take out of the holder's
 * directory, is no attachment.
 */
export const parse = (text: string): ReadonlyArray<Attachment> => {
  const byName = new Map<string, Attachment>()
  for (const raw of text.split("\n")) {
    if (raw.trim() === "") continue
    let parsed: unknown
    try {
      parsed = JSON.parse(raw)
    } catch {
      continue
    }
    const decoded = decodeAttachment(parsed)
    if (decoded._tag === "None" || !isHeld(decoded.value)) continue
    byName.set(decoded.value.name, decoded.value)
  }
  return [...byName.values()]
}

/** One field of a {@link Diff}: the record it belongs to, its name, each column's value, how it reads. */
export const DiffRow = Schema.Struct({
  group: Schema.optionalKey(Schema.String),
  field: Schema.String,
  values: Schema.Array(Schema.NullOr(Schema.String)),
  class: Schema.optionalKey(Schema.String)
})
export interface DiffRow extends Schema.Schema.Type<typeof DiffRow> {}

/** Whether `attachment` stays inside its holder's directory: a `link`, or a relative path with no `..` segment. */
export const isHeld = (attachment: Pick<Attachment, "kind" | "file">): boolean =>
  attachment.kind === "link" ||
  (attachment.file !== "" && !attachment.file.startsWith("/") &&
    !attachment.file.split(/[\\/]/).some((segment) => segment === ".."))

/**
 * A `diff` document: one column per side compared (`expected`, `actual`, or a
 * cell each), one row per field whose values differ, grouped where `group`
 * names the record the field belongs to. A value is null where its column
 * holds no such record. A row's `class` says how its difference reads (the
 * writer's vocabulary); the rows of a class `quiet` names are listed only on
 * demand.
 */
export const Diff = Schema.Struct({
  columns: Schema.Array(Schema.String),
  rows: Schema.Array(DiffRow),
  /** How many fields compared read the same in every column. */
  same: Schema.optionalKey(Schema.Number),
  /** The classes whose rows the site folds until asked. */
  quiet: Schema.optionalKey(Schema.Array(Schema.String))
})
export interface Diff extends Schema.Schema.Type<typeof Diff> {}

/** How the site colours a table cell: `ok` as a pass, `nok` as a failure, `muted` as no value. */
export type CellClass = "ok" | "nok" | "muted"

const CELL_CLASSES: ReadonlySet<string> = new Set<CellClass>(["ok", "nok", "muted"])

/**
 * A `table` attachment's content: its column names, then its rows, each value
 * as text; `classes`, where present, each row's cells' {@link CellClass}
 * aligned with `rows`, null for a plain cell.
 */
export interface Table {
  readonly columns: ReadonlyArray<string>
  readonly rows: ReadonlyArray<ReadonlyArray<string>>
  readonly classes?: ReadonlyArray<ReadonlyArray<CellClass | null>>
}

/** A flat record of a `records` attachment: each field's value as text. */
export type FlatRecord = Readonly<Record<string, string>>

/** `value` as one table or record cell: a string as it is, nothing empty, anything else as JSON. */
const cellText = (value: unknown): string =>
  typeof value === "string" ? value : value === null || value === undefined ? "" : JSON.stringify(value)

/** The JSON values of a JSON-lines `text`, a line that does not parse skipped. */
const jsonLines = (text: string): ReadonlyArray<unknown> =>
  text.split("\n").flatMap((raw) => {
    if (raw.trim() === "") return []
    try {
      return [JSON.parse(raw) as unknown]
    } catch {
      return []
    }
  })

/** `table` as the text of its file: a classed cell as `{"value", "class"}`, any other as its text. */
export const tableText = (table: Table): string =>
  [
    table.columns,
    ...table.rows.map((row, at) =>
      row.map((value, i) => {
        const cls = table.classes?.[at]?.[i] ?? null
        return cls === null ? value : { value, class: cls }
      })
    )
  ].map((row) => `${JSON.stringify(row)}\n`).join("")

/** One cell of a table file: its text, and its class where it states a {@link CellClass}. */
const tableCell = (cell: unknown): { readonly text: string; readonly cls: CellClass | null } => {
  if (typeof cell !== "object" || cell === null || Array.isArray(cell) || !("value" in cell)) {
    return { text: cellText(cell), cls: null }
  }
  const stated = (cell as { readonly class?: unknown }).class
  return {
    text: cellText(cell.value),
    cls: typeof stated === "string" && CELL_CLASSES.has(stated) ? (stated as CellClass) : null
  }
}

/**
 * The table a `table` file's `text` holds: its first array line the columns,
 * each later array line a row; `classes` present where a cell states one.
 */
export const tableOf = (text: string): Table => {
  const [columns = [], ...rows] = jsonLines(text).filter(Array.isArray).map((row) => (row as ReadonlyArray<unknown>).map(tableCell))
  const classes = rows.map((row) => row.map((cell) => cell.cls))
  return {
    columns: columns.map((cell) => cell.text),
    rows: rows.map((row) => row.map((cell) => cell.text)),
    ...(classes.some((row) => row.some((cls) => cls !== null)) ? { classes } : {})
  }
}

/** `records` as the text of their file. */
export const recordsText = (records: ReadonlyArray<FlatRecord>): string =>
  records.map((record) => `${JSON.stringify(record)}\n`).join("")

/** The records a `records` file's `text` holds, each object line one, its values as text. */
export const recordsOf = (text: string): ReadonlyArray<FlatRecord> =>
  jsonLines(text).flatMap((value) =>
    typeof value === "object" && value !== null && !Array.isArray(value)
      ? [Object.fromEntries(Object.entries(value).map(([name, field]) => [name, cellText(field)]))]
      : []
  )

const decodeDiff = Schema.decodeUnknownOption(Diff)

/** `value` as a {@link Diff}; undefined where it is none. */
export const diffOf = (value: unknown): Diff | undefined => {
  const decoded = decodeDiff(value)
  return decoded._tag === "Some" ? decoded.value : undefined
}
