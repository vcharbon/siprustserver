/**
 * The judges a batch runs over its exports alone: a judge whose name a
 * collector running on the lane collects is that collector's, so its name is
 * never judged twice; a cell-scope collector runs on a real-clock lane alone.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as JudgePlan from "../src/judge-plan.js"
import type * as Lanes from "../src/lanes.js"
import * as RigPath from "../src/rig-path.js"
import { laneOf } from "./operations.js"
import { type Entry, run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const SCRIPT: Entry = { text: "#!/bin/sh\nexit 0\n", executable: true }

/** A rig path with the cell-scope collector `probe` and the judges `probe` and `orphan`, the latter no collector's. */
const SAMPLES: Readonly<Record<string, Entry>> = {
  "collectors/probe/collector.toml": `schema = "collector.v1"\nname = "probe"\nscope = "cell"\njudge = "probe"\n[run]\ncollect = "./collect"\n`,
  "collectors/probe/collect": SCRIPT,
  "judges/probe/judge.toml": `schema = "judge.v1"\nname = "probe"\nrun = "./judge"\n`,
  "judges/probe/judge": SCRIPT,
  "judges/orphan/judge.toml": `schema = "judge.v1"\nname = "orphan"\nrun = "./judge"\n`,
  "judges/orphan/judge": SCRIPT
}

const REAL: Lanes.Loaded = { ...laneOf("lane-r"), lane: { schema: "lane.v1", clock: "real" } }

const loaded = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const read = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(read)) throw new Error(`the samples do not load: ${String(read.failure)}`)
  return { root, rig: read.success }
}

describe("the judges of a batch's exports alone", () => {
  it("leave out on a real-clock lane the name a cell-scope collector judges, and judge it on a simulated lane, where that collector runs on no cell", async () => {
    const { root, rig } = await loaded()
    const names = (lane: Lanes.Loaded) => JudgePlan.exportJudgesOn(rig, lane, path.join(root, "batch")).judges.map((one) => one.judge.name)
    expect(names(REAL)).toEqual(["orphan"])
    expect(names(laneOf("lane-s"))).toEqual(["orphan", "probe"])
  })
})
