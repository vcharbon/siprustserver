/**
 * The join (ADR-0060 appendix §6): the records of one name matched to the
 * cells of a scope by equality on one key, in one pass over the records. The
 * cells' values of the key are indexed first; each record is then looked up
 * once. A record keyed by a value matches every cell whose key holds it; a
 * record assigned to a cell names it by its id. Matching one cell, it is
 * joined to it; none, it is *unclaimed*; several, it is *ambiguous* and
 * joined to none. Unclaimed and ambiguous records are kept, never dropped.
 */
import type * as Cell from "./cell.js"

/** A record's fields, opaque to the base. */
export type Fields = Readonly<Record<string, unknown>>

/** A record the base joins by the key's value. */
export interface Keyed {
  readonly record: Fields
  readonly key: string
}

/**
 * A record assigned to a cell: the cell whose id is `cell`, or, where
 * `resolved` is stated, the cells whose ids it lists (`cell` is then the name
 * its writer gave the cell by, an export's directory name).
 */
export interface Assigned {
  readonly record: Fields
  readonly cell: string
  readonly resolved?: ReadonlyArray<string>
}

/** One record of a name, as the join reads it. */
export type Entry = Keyed | Assigned

/** A record joined to no cell, as its writer stated it; an ambiguous one with the ids of the cells it matched. */
export type Unjoined =
  | Keyed & { readonly cells?: ReadonlyArray<string> }
  | { readonly record: Fields; readonly cell: string; readonly cells?: ReadonlyArray<string> }

/** The records of a name over a scope's cells. */
export interface Joined {
  /** Per cell of the scope, by id, the records joined to it, in the order read; empty where none. */
  readonly cells: ReadonlyMap<string, ReadonlyArray<Fields>>
  readonly unclaimed: ReadonlyArray<Unjoined>
  readonly ambiguous: ReadonlyArray<Unjoined>
}

const isKeyed = (entry: Entry): entry is Keyed => "key" in entry

/** Per value of the key `key` among `cells`, the ids of the cells holding it, each once. */
const indexOf = (key: string, cells: ReadonlyArray<Pick<Cell.Cell, "id" | "keys">>): ReadonlyMap<string, ReadonlyArray<string>> => {
  const index = new Map<string, Array<string>>()
  for (const cell of cells) {
    for (const value of new Set(cell.keys[key] ?? [])) {
      const holders = index.get(value)
      if (holders === undefined) index.set(value, [cell.id])
      else holders.push(cell.id)
    }
  }
  return index
}

/**
 * `records` joined to `cells` on the cell key named `key`: `records` is
 * iterated once. `key` is `undefined` where the manifest names none; a keyed
 * record is then unclaimed.
 */
export const join = (
  key: string | undefined,
  cells: ReadonlyArray<Pick<Cell.Cell, "id" | "keys">>,
  records: Iterable<Entry>
): Joined => {
  const index = key === undefined ? new Map<string, ReadonlyArray<string>>() : indexOf(key, cells)
  const ids = new Set(cells.map((cell) => cell.id))
  const joined = new Map<string, Array<Fields>>(cells.map((cell) => [cell.id, []]))
  const unclaimed: Array<Unjoined> = []
  const ambiguous: Array<Unjoined> = []
  for (const entry of records) {
    const matched = isKeyed(entry) ? index.get(entry.key) ?? [] : (entry.resolved ?? [entry.cell]).filter((id) => ids.has(id))
    if (matched.length === 1) {
      joined.get(matched[0]!)!.push(entry.record)
      continue
    }
    const stated: Unjoined = isKeyed(entry) ? { record: entry.record, key: entry.key } : { record: entry.record, cell: entry.cell }
    if (matched.length === 0) unclaimed.push(stated)
    else ambiguous.push({ ...stated, cells: matched })
  }
  return { cells: joined, unclaimed, ambiguous }
}
