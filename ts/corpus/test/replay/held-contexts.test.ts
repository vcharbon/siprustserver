/**
 * A batch's call-contexts reading lands on every run its cells ran under: a
 * context one cell's call left on that cell's run alone, charged to it; one
 * no single cell's call names on every run, as the batch's.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import type { ContextsReport } from "../../src/replay/batch-protocol.js"
import * as HeldContexts from "../../src/replay/held-contexts.js"

let root: string

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "held-contexts-"))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const lines = (runDir: string): ReadonlyArray<HeldContexts.Line> =>
  fs.readFileSync(path.join(runDir, HeldContexts.FILE), "utf8").trim().split("\n").map((line) => JSON.parse(line))

describe("HeldContexts", () => {
  const one = () => path.join(root, "x", "run1")
  const two = () => path.join(root, "y", "run2")
  const report = (): ContextsReport => ({
    store: "127.0.0.1:6909",
    before: 3,
    after: 6,
    held: [
      {
        ref_id: "sha1",
        call_ids: ["A-1@pivot.invalid", "term1-A-1@pivot.invalid"],
        cells: [`${one()}/x-case1__pivot-replay__real`]
      },
      { ref_id: "sha2", call_ids: [], cells: [] },
      {
        ref_id: "sha3",
        call_ids: ["A-2@pivot.invalid"],
        cells: [`${one()}/x-case2__pivot-replay__real`, `${two()}/y-case1__pivot-replay__real`]
      }
    ]
  })

  it("keeps a cell's context on its own run, and the batch's on every run", async () => {
    await Effect.runPromise(
      HeldContexts.keep(report(), [one(), two(), one()], "2026-10-14T10:00:00.000Z").pipe(Effect.provide(NodeServices.layer))
    )
    expect(lines(one())).toEqual([{
      at: "2026-10-14T10:00:00.000Z",
      store: "127.0.0.1:6909",
      before: 3,
      after: 6,
      held: [
        {
          ref_id: "sha1",
          call_ids: ["A-1@pivot.invalid", "term1-A-1@pivot.invalid"],
          cells: ["x-case1__pivot-replay__real"],
          charged: true
        },
        { ref_id: "sha2", call_ids: [], cells: [], charged: false },
        { ref_id: "sha3", call_ids: ["A-2@pivot.invalid"], cells: ["x-case2__pivot-replay__real"], charged: false }
      ]
    }])
    expect(lines(two()).map((line) => line.held.map((held) => held.ref_id))).toEqual([["sha2", "sha3"]])
    // A second batch appends its own line.
    await Effect.runPromise(
      HeldContexts.keep({ store: "127.0.0.1:6909", unread: "connect refused", held: [] }, [two()], "t2").pipe(
        Effect.provide(NodeServices.layer)
      )
    )
    expect(lines(two()).map((line) => line.unread)).toEqual([undefined, "connect refused"])
  })

  it("charges a context one cell's call left to that cell's run alone", () => {
    expect(HeldContexts.charges(report(), [one(), two()])).toEqual([{
      runDir: one(),
      cell: "x-case1__pivot-replay__real",
      finding: "cell x-case1__pivot-replay__real: the SUT at 127.0.0.1:6909 still holds call context " +
        "sha1 of its call after its batch (expected none)"
    }])
  })
})
