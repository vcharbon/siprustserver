/**
 * A collector's collect whose reading in the base dies (the check of an
 * attachment line throws, as `fs.statSync` does where the file goes between
 * its check and its stat): that collector `not checkable`, saying it died, on
 * every cell and the scope; the scope's other collectors' answers kept.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, expect, it, vi } from "vitest"
import * as CollectorPlan from "../src/collector-plan.js"
import * as CollectorRun from "../src/collector-run.js"
import * as RigPath from "../src/rig-path.js"
import { cellOf, memoryLocker } from "./collectors.js"
import { laneOf } from "./operations.js"
import { type Entry, run, tree } from "./tree.js"

vi.mock("../src/attached.js", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/attached.js")>()),
  faultOf: () => {
    throw new Error("the attached file went away")
  }
}))

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const LANE = laneOf("lane-a", {})

/** The files of a batch collector `name` answering `name`, whose collect prints `lines` on its stdout. */
const collector = (name: string, lines: ReadonlyArray<object>): Record<string, Entry> => ({
  [`collectors/${name}/collector.toml`]: `schema = "collector.v1"\nname = "${name}"\nscope = "batch"\nanswers = ["${name}"]\n[run]\ncollect = "./collect"\n`,
  [`collectors/${name}/collect`]: { text: `#!/bin/sh\n${lines.map((line) => `echo '${JSON.stringify(line)}'`).join("\n")}\n`, executable: true }
})

it("is not checkable on every cell and the scope where the base's own reading of a collector's lines dies, the other collectors' answers kept", async () => {
  const root = tree({
    ...collector("attaches", [{ attachment: { kind: "log", file: "seen.log" } }, { verification: "attaches", answer: "pass" }]),
    ...collector("checks", [{ verification: "checks", cell: "c1", answer: "pass" }, { verification: "checks", cell: "c2", answer: "pass" }])
  })
  made.push(root)
  const read = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(read)) throw new Error(`the path does not load: ${String(read.failure)}`)
  const holder = path.join(root, "batch")
  const instances = CollectorPlan.batchPlanOf(read.success, LANE, path.join(holder, "collectors"), holder)
  const cells = ["c1", "c2"].map((id) => cellOf(id, LANE.id, path.join(root, "cells", id)))
  const said: Array<string> = []
  const options: CollectorRun.Options = {
    sink: (event) => Effect.sync(() => void said.push(`${event.phase} ${event.collector}${event.message === undefined ? "" : `: ${event.message}`}`)),
    locker: memoryLocker()
  }
  const collected = await Effect.runPromise(Effect.scoped(Effect.flatMap(CollectorRun.opened(instances, options), (opened) => opened.collect({ cells, unread: [], failed: [] }))))
  const byLabel = Object.fromEntries(collected.map((one) => [
    one.label,
    { cells: Object.fromEntries([...one.cells].map(([id, held]) => [id, held.verifications])), scope: one.scope.verifications }
  ]))
  const died = [{ name: "attaches", source: "attaches", answer: "not-checkable", message: "collect died: the attached file went away" }]
  const passed = [{ name: "checks", source: "checks", answer: "pass" }]
  expect(byLabel).toEqual({
    attaches: { cells: { c1: died, c2: died }, scope: died },
    checks: { cells: { c1: passed, c2: passed }, scope: [] }
  })
  expect(said).toContain("failed attaches: collect died: the attached file went away")
})
