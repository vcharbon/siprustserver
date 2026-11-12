/**
 * A scope's exports (ADR-0060 appendix §4): what a cell's or the scope's own
 * process left under a collector's name, `exports/<name>.jsonl` in the cell's
 * directory or in the scope's. Its lines are record and key lines (§5).
 *
 * An export names a cell by the name of the cell's directory: the process
 * that writes it knows where it writes, not the id the scope reads the cell
 * by (a retry's, a repeat's). The base reads that name as the scope's cell
 * whose directory bears it: in a cell's export, that cell alone; in the
 * scope's, every cell of the scope whose directory bears it. A record naming
 * none is unclaimed and one naming several ambiguous, each keeping the name it
 * stated; a key naming other than one cell is dropped, said as a fault.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Cell from "./cell.js"
import * as Lines from "./collector-lines.js"
import type * as Join from "./join.js"

/** The directory of a cell's or a scope's exports. */
export const DIR = "exports"

/** The suffix of an export's file name. */
const SUFFIX = ".jsonl"

/** What the exports of one name hold over a scope. */
export interface Exported {
  /** Its records, each assigned one with the ids of the cells its directory name resolves to. */
  readonly records: ReadonlyArray<Join.Entry>
  /** Its keys, each naming its cell by id. */
  readonly keys: ReadonlyArray<Lines.KeyLine>
}

/** A scope's exports, by name, and what could not be read of them. */
export interface Read {
  readonly byName: ReadonlyMap<string, Exported>
  /** Each file or line skipped, and why. */
  readonly faults: ReadonlyArray<string>
}

/** The export files under `dir`, by name. */
const filesUnder = (dir: string): ReadonlyArray<readonly [string, string]> => {
  const exports = path.join(dir, DIR)
  if (!fs.existsSync(exports)) return []
  return fs.readdirSync(exports)
    .filter((file) => file.endsWith(SUFFIX))
    .sort()
    .map((file) => [file.slice(0, -SUFFIX.length), path.join(exports, file)] as const)
}

/** The text of `file`, or why it could not be read. */
const textOf = (file: string): Result.Result<string, string> => {
  try {
    return Result.succeed(fs.readFileSync(file, "utf8"))
  } catch (cause) {
    return Result.fail(`${file}: ${cause instanceof Error ? cause.message : String(cause)}`)
  }
}

/** A resolver of a cell directory's name to the ids of the cells of `cells` whose directory bears it. */
const resolverOf = (cells: ReadonlyArray<Pick<Cell.Cell, "id" | "dir">>) => {
  const byDir = new Map<string, Array<string>>()
  for (const cell of cells) {
    const name = path.basename(cell.dir)
    byDir.set(name, [...(byDir.get(name) ?? []), cell.id])
  }
  return (name: string): ReadonlyArray<string> => byDir.get(name) ?? []
}

/**
 * The exports of `cells` and of the scope whose directory is `holder`, read
 * once: each cell's resolved against that cell alone, the scope's against
 * every cell.
 */
export const read = (cells: ReadonlyArray<Pick<Cell.Cell, "id" | "dir">>, holder: string): Read => {
  const byName = new Map<string, { records: Array<Join.Entry>; keys: Array<Lines.KeyLine> }>()
  const faults: Array<string> = []
  const scopeResolver = resolverOf(cells)
  const sources = [
    ...cells.map((cell) => [cell.dir, resolverOf([cell])] as const),
    [holder, scopeResolver] as const
  ]
  for (const [dir, resolve] of sources) {
    for (const [name, file] of filesUnder(dir)) {
      const parsed = Result.flatMap(textOf(file), (text) => Lines.parse(text, file))
      if (Result.isFailure(parsed)) {
        faults.push(`${parsed.failure}: the export is skipped`)
        continue
      }
      const held = byName.get(name) ?? { records: [], keys: [] }
      byName.set(name, held)
      for (const { at, line } of parsed.success) {
        switch (line._tag) {
          case "KeyedRecord":
            held.records.push(line.line)
            break
          case "AssignedRecord":
            held.records.push({ record: line.line.record, cell: line.line.cell, resolved: resolve(line.line.cell) })
            break
          case "Key": {
            const ids = resolve(line.line.cell)
            if (ids.length === 1) held.keys.push({ ...line.line, cell: ids[0]! })
            else faults.push(`${at}: names the cell directory ${line.line.cell}, which no one cell of the scope bears: the key is dropped`)
            break
          }
          default:
            faults.push(`${at}: an export holds records and keys only: the line is skipped`)
        }
      }
    }
  }
  return { byName, faults }
}
