/**
 * Which collectors a batch and a job run: a batch one instance of each
 * collector of its scope wired on its lane (its parts present, not excluded),
 * `at = "after"` and `always`; a job one instance of a job-scope collector
 * per distinct set of the part files it needs, over the cells of the lanes
 * naming that set, and one over every lane where it needs none. Parameters
 * are the manifest's after the lane's overrides.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CollectorPlan from "../src/collector-plan.js"
import type * as Lanes from "../src/lanes.js"
import * as RigPath from "../src/rig-path.js"
import { SAMPLES } from "./collectors.js"
import { laneOf } from "./operations.js"
import { run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const samples = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  return loaded.success
}

const STORE = "records-store"

/** `lane` with the `[collectors]` table `collectors`. */
const wiring = (lane: Lanes.Loaded, collectors: Record<string, unknown>): Lanes.Loaded => ({
  ...lane,
  lane: { ...lane.lane, collectors: collectors as never }
})

describe("the collectors a batch runs", () => {
  it("are those of batch scope wired on its lane, after its cells and always, by name", async () => {
    const rig = await samples()
    const withStore = CollectorPlan.batchPlanOf(rig, laneOf("a", { [STORE]: "targets/x.toml" }), "/b/collectors", "/b")
    const without = CollectorPlan.batchPlanOf(rig, laneOf("d"), "/b/collectors", "/b")
    const always = [
      "also-locked",
      "failing-close",
      "failing-collect",
      "failing-open",
      "flaky-close",
      "hanging-collect",
      "leaves-process",
      "locked",
      "lost-file",
      "not-json",
      "slow-open",
      "stranger",
      "wrong-cell"
    ]
    expect(withStore.map((instance) => instance.id)).toEqual([...always.slice(0, 12), "tidy", "wrong-cell"])
    expect(without.map((instance) => instance.id)).toEqual(always)
    expect(withStore.find((instance) => instance.id === "tidy")).toMatchObject({
      label: "tidy",
      scope: "batch",
      lanes: undefined,
      params: { poll_s: 1 },
      dir: path.join("/b/collectors", "tidy"),
      holder: "/b"
    })
  })

  it("leaves out a collector its lane excludes, and takes the lane's override of a parameter", async () => {
    const rig = await samples()
    const lane = laneOf("a", { [STORE]: "targets/x.toml" })
    const excluded = CollectorPlan.batchPlanOf(rig, wiring(lane, { exclude: ["tidy"] }), "/b", "/b")
    const tuned = CollectorPlan.batchPlanOf(rig, wiring(lane, { tidy: { poll_s: 5 } }), "/b", "/b")
    expect(excluded.map((instance) => instance.id)).not.toContain("tidy")
    expect(tuned.find((instance) => instance.id === "tidy")?.params).toEqual({ poll_s: 5 })
  })
})

describe("the collectors a job runs", () => {
  it("run once per distinct part file among the job's lanes, over the lanes naming it, and once where they need no part", async () => {
    const rig = await samples()
    const lanes = [
      laneOf("a", { [STORE]: "targets/x.toml" }),
      laneOf("b", { [STORE]: "targets/x.toml" }),
      laneOf("c", { [STORE]: "targets/y.toml" }),
      laneOf("d")
    ]
    const planned = CollectorPlan.jobPlanOf(rig, lanes, "/j/collectors", "/j")
    expect(planned.map((instance) => [instance.id, instance.label, instance.lane?.id, instance.lanes])).toEqual([
      ["job-plain", "job-plain", undefined, ["a", "b", "c", "d"]],
      ["job-wide-a", "job-wide for a", "a", ["a", "b"]],
      ["job-wide-c", "job-wide for c", "c", ["c"]]
    ])
    expect(planned.every((instance) => instance.scope === "job" && instance.holder === "/j")).toBe(true)
  })
})
