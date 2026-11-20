/**
 * The join matches a name's records to a scope's cells by equality on one key
 * in a single pass over the records, counted from outside by the iterable it
 * is handed; a record matching two cells is ambiguous and joined to neither,
 * one matching none unclaimed, and a record assigned to a cell bypasses the
 * key.
 */
import { describe, expect, it } from "vitest"
import * as Join from "../src/join.js"

/** `entries` as an iterable that counts how often it is iterated and how many entries are read. */
const counted = (entries: ReadonlyArray<Join.Entry>) => {
  const seen = { passes: 0, reads: 0 }
  const iterable: Iterable<Join.Entry> = {
    [Symbol.iterator]: () => {
      seen.passes += 1
      const inner = entries[Symbol.iterator]()
      return {
        next: () => {
          const next = inner.next()
          if (next.done !== true) seen.reads += 1
          return next
        }
      }
    }
  }
  return { seen, iterable }
}

/** A cell `id` whose `call_id` key holds `callIds`. */
const cell = (id: string, callIds: ReadonlyArray<string>) => ({ id, keys: { call_id: callIds } })

describe("the join", () => {
  it("joins 1000 records keyed by Call-ID to ten cells in one pass, each record read once", () => {
    const cells = Array.from({ length: 10 }, (_, c) => cell(`c${c}`, Array.from({ length: 100 }, (_, n) => `call-${c}-${n}@test`)))
    const records = Array.from({ length: 1000 }, (_, i) => ({ record: { seq: i }, key: `call-${i % 10}-${Math.floor(i / 10)}@test` }))
    const { seen, iterable } = counted(records)
    const joined = Join.join("call_id", cells, iterable)
    expect(seen).toEqual({ passes: 1, reads: 1000 })
    expect([...joined.cells].map(([id, list]) => [id, list.length])).toEqual(cells.map((one) => [one.id, 100]))
    expect(joined.cells.get("c3")!.slice(0, 2)).toEqual([{ seq: 3 }, { seq: 13 }])
    expect([joined.unclaimed, joined.ambiguous]).toEqual([[], []])
  })

  it("keeps a record two cells' keys hold as ambiguous, joined to neither, and one no cell holds as unclaimed", () => {
    const joined = Join.join("call_id", [cell("c1", ["a@x", "shared@x"]), cell("c2", ["shared@x"])], [
      { record: { n: 1 }, key: "shared@x" },
      { record: { n: 2 }, key: "a@x" },
      { record: { n: 3 }, key: "nobody@x" }
    ])
    expect(Object.fromEntries(joined.cells)).toEqual({ c1: [{ n: 2 }], c2: [] })
    expect(joined.ambiguous).toEqual([{ record: { n: 1 }, key: "shared@x", cells: ["c1", "c2"] }])
    expect(joined.unclaimed).toEqual([{ record: { n: 3 }, key: "nobody@x" }])
  })

  it("joins an assigned record to the cell it names whatever its keys, and keeps one naming no cell of the scope unclaimed", () => {
    const joined = Join.join("call_id", [cell("c1", []), cell("c2", [])], [
      { record: { n: 1 }, cell: "c2" },
      { record: { n: 2 }, cell: "c9" },
      { record: { n: 3 }, cell: "c1~2", resolved: ["c1"] },
      { record: { n: 4 }, cell: "twin", resolved: ["c1", "c2"] }
    ])
    expect(Object.fromEntries(joined.cells)).toEqual({ c1: [{ n: 3 }], c2: [{ n: 1 }] })
    expect(joined.unclaimed).toEqual([{ record: { n: 2 }, cell: "c9" }])
    expect(joined.ambiguous).toEqual([{ record: { n: 4 }, cell: "twin", cells: ["c1", "c2"] }])
  })

  it("leaves a keyed record unclaimed where the manifest names no key", () => {
    const joined = Join.join(undefined, [cell("c1", ["a@x"])], [{ record: { n: 1 }, key: "a@x" }])
    expect([joined.cells.get("c1"), joined.unclaimed]).toEqual([[], [{ record: { n: 1 }, key: "a@x" }]])
  })
})
