/**
 * A scope's collectors run after those their `after` names among the ones
 * that run with them, else by name; a name not running orders nothing; a
 * cycle of `after` among one scope's collectors refuses the path at load.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import { afterEach, describe, expect, it } from "vitest"
import * as CollectorPlan from "../src/collector-plan.js"
import * as RigPath from "../src/rig-path.js"
import { laneOf } from "./operations.js"
import { collector, run, SCRIPT, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** The path of collectors named by `afters`' keys, each `after` its value, loaded. */
const load = (afters: Readonly<Record<string, ReadonlyArray<string>>>) => {
  const files = Object.fromEntries(Object.entries(afters).flatMap(([name, after]) => [
    [`collectors/${name}/collector.toml`, collector(name, after.length === 0 ? "" : `after = [${after.map((one) => `"${one}"`).join(", ")}]`)],
    [`collectors/${name}/collect`, SCRIPT]
  ]))
  const root = tree(files)
  made.push(root)
  return run(RigPath.load({ workspace: root }))
}

describe("the order of a scope's collectors", () => {
  it("runs each after those its after names, else by name, a name that does not run ordering nothing", async () => {
    const loaded = await load({ alpha: ["zulu"], beta: [], mike: ["alpha", "absent"], zulu: [] })
    if (!Result.isSuccess(loaded)) throw new Error(String(loaded.failure))
    const planned = CollectorPlan.batchPlanOf(loaded.success, laneOf("lane-a", {}), "/tmp/collectors", "/tmp/batch")
    expect(planned.map((instance) => instance.id)).toEqual(["beta", "zulu", "alpha", "mike"])
  })

  it("refuses a path whose collectors' after makes a cycle, naming each manifest", async () => {
    const loaded = await load({ alpha: ["beta"], beta: ["gamma"], gamma: ["alpha"], delta: [] })
    expect(Result.isFailure(loaded) && loaded.failure.reason).toBe("after: alpha runs after beta runs after gamma runs after alpha: a cycle, so none of them runs first")
    expect(Result.isFailure(loaded) && loaded.failure.files.map((file) => file.split("/").slice(-2, -1)[0])).toEqual(["alpha", "beta", "gamma"])
  })
})
