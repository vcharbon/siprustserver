/**
 * What a scope adds to its cells' descriptions as it runs (ADR-0060 appendix
 * §5, §6): the keys its exports and collectors add, and per name the records
 * joined to each cell. Both are kept in the cell's directory, so a wider
 * scope, describing the cell anew off it (`cell-of-bundle.ts`), reads them:
 * {@link KEYS_FILE}, every added key's values, and `joined/<name>.jsonl`, one
 * record per line, the file `cell.v1`'s `records` names.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Cell from "./cell.js"
import type * as Lines from "./collector-lines.js"
import type * as Join from "./join.js"

/** The file of a cell's directory holding the keys added to it: key name to its values. */
export const KEYS_FILE = "keys.json"

/** The directory of a cell's directory, and of a scope's, holding the records joined to it, one file per name. */
export const JOINED_DIR = "joined"

/** The file of the records of `name` joined to the cell or kept on the scope whose directory is `dir`. */
export const joinedFileOf = (dir: string, name: string): string => path.join(dir, JOINED_DIR, `${name}.jsonl`)

/** `values` added to `into`'s, each once, in the order first seen. */
const mergedValues = (into: ReadonlyArray<string> | undefined, values: ReadonlyArray<string>): Array<string> => [
  ...new Set([...(into ?? []), ...values])
]

/** The keys a {@link KEYS_FILE}'s `text` holds; none where it is absent or no object of string lists, each entry that is none skipped. */
export const addedKeysOf = (text: string | undefined): Record<string, Array<string>> => {
  if (text === undefined) return {}
  try {
    const raw: unknown = JSON.parse(text)
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return {}
    return Object.fromEntries(
      Object.entries(raw).flatMap(([name, values]) =>
        Array.isArray(values) && values.every((value) => typeof value === "string") ? [[name, values as Array<string>]] : []
      )
    )
  } catch {
    return {}
  }
}

/** `lines` added to the keys of `cell`, kept in its {@link KEYS_FILE} beside those added before. */
const keyed = (cell: Cell.Cell, lines: ReadonlyArray<Lines.KeyLine>): Cell.Cell => {
  const file = path.join(cell.dir, KEYS_FILE)
  const kept = addedKeysOf(fs.existsSync(file) ? fs.readFileSync(file, "utf8") : undefined)
  const keys: Record<string, Array<string>> = { ...cell.keys } as Record<string, Array<string>>
  for (const line of lines) {
    kept[line.key] = mergedValues(kept[line.key], line.values)
    keys[line.key] = mergedValues(keys[line.key], line.values)
  }
  fs.mkdirSync(cell.dir, { recursive: true })
  fs.writeFileSync(file, `${JSON.stringify(kept, null, 2)}\n`)
  return { ...cell, keys }
}

/** `cells` with the keys `lines` add to each, each cell named kept in its directory. */
export const withKeys = (cells: ReadonlyArray<Cell.Cell>, lines: ReadonlyArray<Lines.KeyLine>): ReadonlyArray<Cell.Cell> => {
  if (lines.length === 0) return cells
  const byCell = new Map<string, Array<Lines.KeyLine>>()
  for (const line of lines) byCell.set(line.cell, [...(byCell.get(line.cell) ?? []), line])
  return cells.map((cell) => {
    const mine = byCell.get(cell.id)
    return mine === undefined ? cell : keyed(cell, mine)
  })
}

/** `records` written to `file`, one JSON line each; the file emptied where there is none. */
export const writeLines = (file: string, records: ReadonlyArray<unknown>): void => {
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, records.map((record) => `${JSON.stringify(record)}\n`).join(""))
}

/**
 * `cells` with the records of `name` that `joined` gives each written to its
 * `joined/<name>.jsonl` and named by its `records`; a cell `joined` does not
 * state keeps what it had.
 */
export const withRecords = (cells: ReadonlyArray<Cell.Cell>, name: string, joined: Join.Joined): ReadonlyArray<Cell.Cell> =>
  cells.map((cell) => {
    const records = joined.cells.get(cell.id)
    if (records === undefined) return cell
    const file = joinedFileOf(cell.dir, name)
    writeLines(file, records)
    return { ...cell, records: { ...cell.records, [name]: file } }
  })
