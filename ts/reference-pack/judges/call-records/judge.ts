/**
 * The `call-records` judge: for each cell claiming `{ count, checks? }` (the
 * case form's call-records claim), `pass` where that many records were
 * joined to it and some record satisfies each check, else `fail` stating
 * what does not hold. A check names a field by its path in the record
 * (`b_legs.0.disposition`), compares its text by `op` (`eq`, `regex`,
 * `exists`, `absent`) to a literal `value`, and gates. A claim stating no
 * whole count is `not-checkable`; a cell claiming nothing gets no line.
 *
 * To each claiming cell it attaches its records flattened, one field per
 * value (`records`, named after the judge), and its claimed count and checks
 * against the joined records (`diff`, `<name>-diff`): a row per one that
 * does not hold, per record where several were joined, `same` counting the
 * others.
 */
import type { JudgeRun } from "@sip/rig"
import { existsSync, readFileSync, writeFileSync } from "node:fs"
import * as path from "node:path"

export const NAME = "call-records"

/** A record flattened: each value's path, its text. */
type Flat = Readonly<Record<string, string>>

/** The records joined to a cell, one JSON line each, in the file at `file`; none where it is absent. */
const recordsIn = (file: string | undefined): ReadonlyArray<unknown> =>
  file === undefined || !existsSync(file)
    ? []
    : readFileSync(file, "utf8").split("\n").filter((line) => line.trim() !== "").map((line) => JSON.parse(line) as unknown)

/** `value` as the text a flat record holds: a string as it is, anything else as JSON. */
const textOf = (value: unknown): string => (typeof value === "string" ? value : JSON.stringify(value))

/** `value` flattened under `prefix`: each leaf, and each empty object or array, at its dotted path. */
const flatten = (value: unknown, prefix = ""): ReadonlyArray<readonly [string, string]> => {
  const entries = typeof value === "object" && value !== null ? Object.entries(value) : []
  if (entries.length === 0) return prefix === "" ? [] : [[prefix, textOf(value)]]
  return entries.flatMap(([key, inner]) => flatten(inner, prefix === "" ? key : `${prefix}.${key}`))
}

/** One check of a claim, as the case form states it. */
interface Check {
  readonly field: string
  readonly op: "eq" | "regex" | "exists" | "absent"
  readonly value?: string
}

/** What a claim states: its whole count, and its checks over the records. */
interface Claim {
  readonly count: number
  readonly checks: ReadonlyArray<Check>
}

/** What `claim` states, where it states a whole count. */
const claimOf = (claim: unknown): Claim | undefined => {
  const stated = typeof claim === "object" && claim !== null ? (claim as { count?: unknown; checks?: unknown }) : {}
  const count = stated.count
  if (typeof count !== "number" || !Number.isInteger(count) || count < 0) return undefined
  return { count, checks: Array.isArray(stated.checks) ? (stated.checks as ReadonlyArray<Check>) : [] }
}

/** Whether `record` satisfies `check`. */
const holds = (check: Check, record: Flat): boolean => {
  const field = record[check.field]
  switch (check.op) {
    case "eq":
      return field === check.value
    case "regex":
      return field !== undefined && new RegExp(check.value ?? "").test(field)
    case "exists":
      return field !== undefined
    case "absent":
      return field === undefined
  }
}

/** What `check` claims, as its diff column shows it. */
const claimedOf = (check: Check): string =>
  check.op === "eq" ? check.value ?? "" : check.op === "regex" ? `/${check.value ?? ""}/` : `(${check.op})`

/** One row of a `diff` (`@sip/contracts` `Diff`). */
interface Row {
  readonly group?: string
  readonly field: string
  readonly values: ReadonlyArray<string | null>
}

/**
 * `claim` against `records`: a row for a count that differs, a row per
 * record for each check no record satisfies (one against nothing where none
 * was joined), and how many of the count and checks hold.
 */
const compare = (claim: Claim, records: ReadonlyArray<Flat>): { readonly rows: ReadonlyArray<Row>; readonly same: number } => {
  const countHolds = records.length === claim.count
  const failing = claim.checks.filter((check) => !records.some((record) => holds(check, record)))
  const compared: ReadonlyArray<Flat | undefined> = records.length === 0 ? [undefined] : records
  const rows: ReadonlyArray<Row> = [
    ...(countHolds ? [] : [{ field: "count", values: [String(claim.count), String(records.length)] }]),
    ...failing.flatMap((check) =>
      compared.map((record, at) => ({
        ...(compared.length > 1 ? { group: `record ${at + 1}` } : {}),
        field: check.field,
        values: [claimedOf(check), record?.[check.field] ?? null]
      }))
    )
  ]
  return { rows, same: (countHolds ? 1 : 0) + claim.checks.length - failing.length }
}

/** The lines of `cell` claiming `claim`, its attached files written under `out`. */
const judgeCell = (cell: JudgeRun.Input["context"]["cells"][number], claim: Claim, out: string): ReadonlyArray<object> => {
  const file = cell.records[NAME]
  const records = recordsIn(file === undefined ? undefined : path.resolve(cell.dir, file)).map((record) => Object.fromEntries(flatten(record)))
  const { rows, same } = compare(claim, records)
  const failing = [...new Set(rows.filter((row) => row.field !== "count").map((row) => row.field))]
  const recordsFile = `${cell.id}.records.jsonl`
  const diffFile = `${cell.id}.diff.json`
  writeFileSync(path.join(out, recordsFile), records.map((record) => `${JSON.stringify(record)}\n`).join(""))
  writeFileSync(path.join(out, diffFile), `${JSON.stringify({ columns: ["claimed", "joined"], rows, same })}\n`)
  return [
    {
      verification: NAME,
      cell: cell.id,
      answer: rows.length === 0 ? "pass" : "fail",
      message: `${records.length} record(s), ${claim.count} claimed` +
        (failing.length === 0 ? "" : `; no record satisfies the check on ${failing.join(", ")}`),
      detail: { expected: String(claim.count), seen: String(records.length) }
    },
    { attachment: { name: NAME, kind: "records", title: "the call records joined to the cell", file: recordsFile }, cell: cell.id },
    {
      attachment: { name: `${NAME}-diff`, kind: "diff", title: "the claimed call records against the joined ones", file: diffFile },
      cell: cell.id
    }
  ]
}

export default ({ context, out }: JudgeRun.Input) =>
  context.cells.flatMap((cell) => {
    const stated = cell.claims[NAME]
    if (stated === undefined) return []
    const claim = claimOf(stated)
    if (claim === undefined) {
      return [{ verification: NAME, cell: cell.id, answer: "not-checkable", message: `the claim states no whole count: ${JSON.stringify(stated)}` }]
    }
    return judgeCell(cell, claim, out)
  })
