/**
 * A cell's replay over a fixture pack's lane hosting: its run-spec names the
 * cell's document, its bundle and the pack's lane block toward the routing
 * backend; its batch's cells are judged once the pack wrote its additions into
 * each bundle, each handed to the run as it lands, in order, and every one an
 * error where the batch's end did not run.
 */
import { NodeServices } from "@effect/platform-node"
import type * as LaneBatch from "@sip/corpus/replay/lane-batch"
import { LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import type { Cell, CellResult } from "../src/cell.js"
import { type Ended, judgeAll, type Placed, runSpecOf } from "../src/replay.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "runner-replay-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

const SIM = LaneView.simulatedBuiltIn("sim", {})

const cell = (id: string): Cell => ({
  id,
  ref: `cases/${id}`,
  subcase: "s",
  retries: 0,
  source: { file: `cases/${id}.ts`, text: "" },
  checked: [],
  locks: [],
  needs: [],
  tags: [],
  alone: false,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  dir: `/cells/${id}`,
  endpoints: ["alice", "bob"],
  egress: "bob"
})

/** A pack whose lane block names the lane, the backend and the cell, and which writes an `ok` verdict into a bundle once its lane ended. */
const HOSTING = {
  laneBlock: (found: LaneView.View, routing: string, c: Cell) => ({ pack_lane: found.id, backend: routing, for: c.id }),
  bundled: (_c: Cell, bundle: string) =>
    Effect.sync(() => {
      fs.mkdirSync(bundle, { recursive: true })
      fs.writeFileSync(path.join(bundle, "verdict.json"), JSON.stringify({ status: "ok" }))
    })
}

const PACK = { typer: () => Effect.succeed([]) }

/** A batch of the simulated lane that ended with `value`, its end judged where `judged`. */
const batchOf = (value: ReadonlyArray<Ended>, judged: boolean): LaneBatch.Ended<ReadonlyArray<Ended>> => ({
  value,
  dirty: undefined,
  gates: [],
  sutDied: undefined,
  deadHost: undefined,
  judged,
  contexts: undefined,
  interrupted: 0,
  hosted: false,
  collected: [],
  leftChanged: []
})

const AT = new Date("2026-11-12T10:00:00Z")

/** The cells `a` and `b` placed in bundles of their own under a fresh directory named `under`. */
const placedUnder = (under: string): ReadonlyArray<Placed> =>
  ["a", "b"].map((id) => ({ cell: cell(id), spec: `${under}/${id}.json`, bundle: path.join(dir, under, id), locks: [] }))

/** The results `judgeAll` lands of `placed` ended as `ended`, in the order they landed. */
const landedOf = async (placed: ReadonlyArray<Placed>, ended: LaneBatch.Ended<ReadonlyArray<Ended>>) => {
  const landed: Array<{ readonly result: CellResult; readonly startedAt: Date }> = []
  const context = { landed: (result: CellResult, startedAt: Date) => Effect.sync(() => void landed.push({ result, startedAt })) }
  await Effect.runPromise(judgeAll(PACK, HOSTING, "sim", placed, ended, context).pipe(Effect.provide(NodeServices.layer)))
  return landed
}

describe("a cell's run-spec", () => {
  it("names its document and bundle, and the pack's lane block toward the routing backend", () => {
    expect(runSpecOf(HOSTING, SIM, "127.0.0.1:9000", cell("a"), "/bundles/a")).toMatchObject({
      case: "/cells/a",
      out_dir: "/bundles/a",
      lane: { pack_lane: "sim", backend: "127.0.0.1:9000", for: "a" }
    })
    expect(runSpecOf(HOSTING, SIM, "127.0.0.1:9000", cell("a"), "/bundles/a").run).not.toHaveProperty("endpoint_addresses")
  })
})

describe("a batch's cells judged", () => {
  it("read the bundle the pack wrote into once the lane ended, each landed in order with when it started", async () => {
    const landed = await landedOf(placedUnder("judged"), batchOf([
      { code: 0, summary: "ok", startedAt: AT },
      { code: 3, summary: "environment: the lane went away", startedAt: AT }
    ], true))
    expect(landed.map(({ result }) => [result.cell.id, result.status, result.verdict, result.message])).toEqual([
      ["a", "pass", "ok", ""],
      ["b", "error", "absent", "environment: the lane went away"]
    ])
    expect(landed.map(({ startedAt }) => startedAt)).toEqual([AT, AT])
  })

  it("are each an error where the batch's end did not run, a cell that would pass included", async () => {
    const landed = await landedOf(placedUnder("unjudged"), batchOf([
      { code: 0, summary: "ok", startedAt: AT },
      { code: 0, summary: "ok", startedAt: AT }
    ], false))
    expect(landed.map(({ result }) => [result.cell.id, result.status])).toEqual([["a", "error"], ["b", "error"]])
    expect(landed[0]!.result.message).toBe("not judged: the batch's end did not run: the batch's end gate wrote no end line")
  })
})
