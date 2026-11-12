/**
 * Which collectors a batch and a job run: a batch one instance of each
 * collector of its scope wired on its lane (its parts present, not excluded)
 * and set `always` by the job's collection level, a pre-check as one
 * `at = "after"`, or set `on-failure` and `at = "after"`; an on-demand run
 * one instance of a collector the level sets `on-demand`, over one lane's
 * cells; a job one instance of a job-scope collector
 * per distinct set of the part files it needs, over the cells of the lanes
 * naming that set, and one over every lane where it needs none. Parameters
 * are the manifest's after the lane's overrides.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as Collection from "../src/collection.js"
import type * as Collector from "../src/collector.js"
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
  it("are those of batch scope wired on its lane, always (pre-checks among them) or on failure, by name", async () => {
    const rig = await samples()
    const withStore = CollectorPlan.batchPlanOf(rig, laneOf("a", { [STORE]: "targets/x.toml" }), "/b/collectors", "/b")
    const without = CollectorPlan.batchPlanOf(rig, laneOf("d"), "/b/collectors", "/b")
    const always = [
      "also-locked",
      "failing-close",
      "failing-collect",
      "failing-open",
      "fails-c2",
      "flaky-close",
      "hanging-collect",
      "leaves-process",
      "locked",
      "lost-file",
      "not-json",
      "on-failure",
      "after-on-failure",
      "on-failure-promising",
      "pre-check",
      "refusing-check",
      "required-blind",
      "required-failing",
      "required-hanging",
      "slow-open",
      "stranger",
      "unsure-check",
      "wrong-cell"
    ]
    expect(withStore.map((instance) => instance.id)).toEqual([...always.slice(0, 21), "tidy", ...always.slice(21)])
    expect(without.map((instance) => instance.id)).toEqual(always)
    expect(without.filter((instance) => instance.onFailure === true).map((instance) => instance.id)).toEqual(["on-failure", "on-failure-promising"])
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

describe("a collection level", () => {
  const collection = {
    quick: { never: ["tidy", "on-failure"], "on-demand": ["stranger"] },
    paranoid: { always: ["on-failure", "on-demand"] }
  }

  it("leaves out the collectors it sets never or on-demand, and runs the ones it sets always on every cell", async () => {
    const rig = await samples()
    const lane = laneOf("a", { [STORE]: "targets/x.toml" })
    const ids = (level: string) =>
      CollectorPlan.batchPlanOf(rig, lane, "/b", "/b", Collection.settingsOf(collection, level)).map((instance) => instance.id)
    expect(ids("quick")).not.toContain("tidy")
    expect(ids("quick")).not.toContain("on-failure")
    expect(ids("quick")).not.toContain("stranger")
    const paranoid = CollectorPlan.batchPlanOf(rig, lane, "/b", "/b", Collection.settingsOf(collection, "paranoid"))
    expect(paranoid.filter((instance) => ["on-failure", "on-demand"].includes(instance.id)).map((instance) => [instance.id, instance.onFailure]))
      .toEqual([["on-demand", undefined], ["on-failure", undefined]])
  })

  it("is the workspace's default where the job names none, each collector's own default where that is standard", () => {
    const of = (manifest: Partial<Collector.Collector>) => ({ ...manifest, schema: "collector.v1", scope: "batch", run: { collect: "./c" } }) as Collector.Collector
    const settings = Collection.settingsOf({ default: "quick", ...collection })
    expect(settings(of({ name: "tidy" }))).toBe("never")
    expect(settings(of({ name: "fails-c2" }))).toBe("always")
    expect(settings(of({ name: "sut-trace", default: "on-demand" }))).toBe("on-demand")
    expect(Collection.settingsOf(collection)(of({ name: "tidy" }))).toBe("always")
    expect(Collection.settingsOf({ quick: { never: ["kept"] } }, "quick")(of({ name: "kept", required: true }))).toBe("always")
  })
})

describe("an on-demand run", () => {
  it("takes one instance of a collector set on-demand and wired on the lane, over that lane's cells", async () => {
    const rig = await samples()
    const lane = laneOf("a", { [STORE]: "targets/x.toml" })
    const planned = CollectorPlan.onDemandOf(rig, "on-demand", lane, "/o/collectors", "/o")
    expect(Result.isSuccess(planned) && planned.success).toMatchObject({
      id: "on-demand",
      scope: "batch",
      lanes: ["a"],
      dir: path.join("/o/collectors", "on-demand"),
      holder: "/o"
    })
    expect(CollectorPlan.onDemandOn(rig, lane)).toEqual(["on-demand"])
  })

  it("refuses a collector the path lacks, a pre-check, one not wired on the lane, and one the level does not set on-demand", async () => {
    const rig = await samples()
    const lane = laneOf("a", { [STORE]: "targets/x.toml" })
    const refusal = (name: string, on = lane, settings?: Collection.Settings) => {
      const planned = CollectorPlan.onDemandOf(rig, name, on, "/o", "/o", settings)
      return Result.isFailure(planned) ? planned.failure : "planned"
    }
    expect(refusal("absent")).toBe("no collector of the path is named absent")
    expect(refusal("pre-check")).toBe("pre-check is a pre-check, which no on-demand run serves")
    expect(refusal("on-demand", laneOf("d"))).toBe("on-demand is not wired on the lane d")
    expect(refusal("tidy")).toBe("tidy is set always, not on-demand")
    expect(refusal("on-demand", lane, Collection.settingsOf({ quick: { never: ["on-demand"] } }, "quick")))
      .toBe("on-demand is set never, not on-demand")
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
      ["job-wide-c", "job-wide for c", "c", ["c"]],
      ["required-check-a", "required-check for a", "a", ["a", "b"]],
      ["required-check-c", "required-check for c", "c", ["c"]]
    ])
    expect(planned.every((instance) => instance.scope === "job" && instance.holder === "/j")).toBe(true)
  })

  it("take an asked pre-check, marked asked, whatever the level sets it, and one the level sets always unasked", async () => {
    const rig = await samples()
    const checks = (asked: ReadonlyArray<string>, level?: Collection.Level) =>
      CollectorPlan.jobPlanOf(rig, [laneOf("d")], "/j/collectors", "/j", asked, Collection.settingsOf(level === undefined ? undefined : { set: level }, "set"))
        .filter((instance) => instance.id === "asked-check")
        .map((instance) => [instance.id, instance.asked])
    expect(checks(["asked-check"])).toEqual([["asked-check", true]])
    expect(checks(["asked-check"], { never: ["asked-check"] })).toEqual([["asked-check", true]])
    expect(checks(["asked-check"], { always: ["asked-check"] })).toEqual([["asked-check", true]])
    expect(checks([], { always: ["asked-check"] })).toEqual([["asked-check", undefined]])
  })
})
