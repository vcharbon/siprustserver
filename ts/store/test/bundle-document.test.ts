/**
 * A cell that wrote no `pivot.json` — the interpreter refused it, or it never
 * ran — still records the document it was asked to replay: the one its
 * run-spec names, a v3 file or a case directory's `scenario.json`. A cell
 * whose spec names nothing that reads records none.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as Bundle from "../src/bundle.js"

const roots: Array<string> = []
afterEach(() => {
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true })
})

const DOCUMENT = { pivot_version: 3, id: "c-case1", steps: [] }

/** A run of three refused cells: one spec naming a case directory, one a file, one naming nothing on disk. */
const run = (): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bundle-document-"))
  roots.push(root)
  const caseDir = path.join(root, "content", "c-case1")
  fs.mkdirSync(caseDir, { recursive: true })
  fs.writeFileSync(path.join(caseDir, "scenario.json"), JSON.stringify(DOCUMENT))
  fs.writeFileSync(path.join(root, "content", "c-case2.v3.json"), JSON.stringify({ ...DOCUMENT, id: "c-case2" }))
  const runDir = path.join(root, "run1")
  const cells = ["c-case1", "c-case2", "c-case3"].map((id) => `${id}__pivot-replay__fake`)
  fs.mkdirSync(path.join(runDir, "specs"), { recursive: true })
  const targets = [caseDir, path.join(root, "content", "c-case2.v3.json"), path.join(root, "content", "absent")]
  cells.forEach((cell, at) => {
    fs.mkdirSync(path.join(runDir, cell), { recursive: true })
    fs.writeFileSync(path.join(runDir, cell, "error.txt"), "refused\n")
    fs.writeFileSync(path.join(runDir, "specs", `${cell}.run-spec.json`), JSON.stringify({ case: targets[at] }))
  })
  fs.writeFileSync(
    path.join(runDir, "campaign.json"),
    JSON.stringify({
      cells: cells.map((cell) => ({
        cell: { case: cell.split("__")[0], shape: "pivot-replay", infra: "fake" },
        passed: false,
        dir: cell,
        error: "replay exited 2 (refused) and left no bundle:"
      }))
    })
  )
  return runDir
}

describe("the document of a cell that wrote none", () => {
  it("is the one its run-spec names: a case directory's scenario.json, or a v3 file", async () => {
    const reading = await Effect.runPromise(Bundle.read(run()).pipe(Effect.provide(NodeServices.layer)))
    expect(reading.cases.map((entry) => [entry.caseId, entry.document])).toEqual([
      ["c-case1", DOCUMENT],
      ["c-case2", { ...DOCUMENT, id: "c-case2" }],
      ["c-case3", null]
    ])
  })
})
