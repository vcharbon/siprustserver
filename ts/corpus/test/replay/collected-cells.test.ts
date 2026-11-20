/**
 * The cells a scope's collectors read, described off their bundles: the
 * claims a case source states beside the document reach the description
 * beside the document's own.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CollectedCells from "../../src/replay/collected-cells.js"

const BUNDLE = path.join(import.meta.dirname, "../../../rig/test/fixtures/simulated-cell")

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A copy of the fixture bundle whose document states `claims`. */
const bundleClaiming = (claims: Readonly<Record<string, unknown>>): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "collected-cells-"))
  made.push(dir)
  fs.cpSync(BUNDLE, dir, { recursive: true })
  const pivot = JSON.parse(fs.readFileSync(path.join(dir, "pivot.json"), "utf8"))
  pivot.postconditions = { claims }
  fs.writeFileSync(path.join(dir, "pivot.json"), JSON.stringify(pivot))
  return dir
}

describe("CollectedCells.describe", () => {
  it("describes a cell with the claims stated beside its document after the document's own", async () => {
    const dir = bundleClaiming({ "store-records": { count: 1 } })
    const over = await Effect.runPromise(
      CollectedCells.describe(
        [{ id: "c1", caseId: "case-1", lane: "lane-a", dir, claims: { stub: { answers: ["a"] } } }],
        "lane-a-1"
      ).pipe(Effect.provide(NodeServices.layer))
    )
    expect(over.unread).toEqual([])
    expect(over.cells.map((cell) => cell.claims)).toEqual([{ "store-records": { count: 1 }, stub: { answers: ["a"] } }])
  })

  it("describes a cell placed with no claim with those of its document alone", async () => {
    const dir = bundleClaiming({ "store-records": { count: 1 } })
    const over = await Effect.runPromise(
      CollectedCells.describe([{ id: "c1", caseId: "case-1", lane: "lane-a", dir }], "lane-a-1").pipe(Effect.provide(NodeServices.layer))
    )
    expect(over.cells.map((cell) => cell.claims)).toEqual([{ "store-records": { count: 1 } }])
  })
})
