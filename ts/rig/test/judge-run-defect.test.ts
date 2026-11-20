/**
 * A judge's run whose reading of the judge's lines dies in the base itself
 * (the check of an attachment line throws, as `fs.statSync` does where the
 * file goes between its check and its stat): `not checkable`, saying the
 * judge died, on every cell and the scope.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, expect, it, vi } from "vitest"
import * as Context from "../src/context.js"
import * as JudgeRun from "../src/judge-run.js"
import * as RigPath from "../src/rig-path.js"
import { cellOf } from "./collectors.js"
import { run, tree } from "./tree.js"

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

it("is not checkable on every cell and the scope where the base's own reading of a judge's lines dies", async () => {
  const root = tree({
    "judges/seer/judge.toml": `schema = "judge.v1"\nname = "seer"\nmodule = "./judge.ts"\n`,
    "judges/seer/judge.ts": `export default () => [{ attachment: { kind: "log", file: "seen.log" } }, { verification: "seer", answer: "pass" }]\n`
  })
  made.push(root)
  const read = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(read)) throw new Error(`the path does not load: ${String(read.failure)}`)
  const judge = read.success.judges.get("seer")!
  const cells = ["c1", "c2"].map((id) => cellOf(id, "lane-a", path.join(root, "cells", id)))
  const answered = await Effect.runPromise(JudgeRun.run({
    judge,
    context: { schema: Context.SCHEMA, scope: "batch", window: { start: { lane_ms: 0, wall_ms: 0 } }, cells, params: {} },
    answers: ["seer"],
    dir: path.join(root, "batch", "judges", "seer"),
    holder: path.join(root, "batch")
  }))
  const died = [{ name: "seer", source: "seer", answer: "not-checkable", message: "judge seer died: the attached file went away" }]
  expect({ cells: Object.fromEntries([...answered.cells].map(([id, held]) => [id, held.verifications])), scope: answered.scope.verifications })
    .toEqual({ cells: { c1: died, c2: died }, scope: died })
})
