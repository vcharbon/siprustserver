/**
 * The `call-records` judge: for each cell claiming `{ count }`, `pass` where
 * that many records were joined to it, else `fail` stating the count seen.
 * A claim stating no whole count is `not-checkable`; a cell claiming nothing
 * gets no line.
 */
import type { JudgeRun } from "@sip/rig"
import { existsSync, readFileSync } from "node:fs"
import * as path from "node:path"

export const NAME = "call-records"

/** The records joined to a cell, one JSON line each, in the file at `file`; none where it is absent. */
const recordsIn = (file: string | undefined): ReadonlyArray<unknown> =>
  file === undefined || !existsSync(file)
    ? []
    : readFileSync(file, "utf8").split("\n").filter((line) => line.trim() !== "").map((line) => JSON.parse(line) as unknown)

/** The count `claim` states, where it states a whole one. */
const countOf = (claim: unknown): number | undefined => {
  const count = typeof claim === "object" && claim !== null ? (claim as { count?: unknown }).count : undefined
  return typeof count === "number" && Number.isInteger(count) && count >= 0 ? count : undefined
}

export default ({ context }: JudgeRun.Input) =>
  context.cells.flatMap((cell) => {
    const claim = cell.claims[NAME]
    if (claim === undefined) return []
    const count = countOf(claim)
    if (count === undefined) {
      return [{ verification: NAME, cell: cell.id, answer: "not-checkable", message: `the claim states no whole count: ${JSON.stringify(claim)}` }]
    }
    const file = cell.records[NAME]
    const seen = recordsIn(file === undefined ? undefined : path.resolve(cell.dir, file)).length
    return [{
      verification: NAME,
      cell: cell.id,
      answer: seen === count ? "pass" : "fail",
      message: `${seen} record(s), ${count} claimed`,
      detail: { expected: String(count), seen: String(seen) }
    }]
  })
