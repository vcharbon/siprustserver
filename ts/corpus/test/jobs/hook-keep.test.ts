/**
 * A job's hook logs are kept with each run it made, cut to the
 * window its cells drove the SUT in; a run whose cells state no window keeps
 * none.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as HookKeep from "../../src/jobs/hook-keep.js"

let root: string

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "hook-keep-"))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const at = (iso: string) => Date.parse(iso)

/** A cell directory under `runDir`, stating the window its calls ran in where `window` is given. */
const cell = (runDir: string, name: string, window?: [string, string]) => {
  const dir = path.join(runDir, name)
  fs.mkdirSync(dir, { recursive: true })
  if (window !== undefined) {
    fs.writeFileSync(
      path.join(dir, "sut-join.json"),
      JSON.stringify({ call_ids: [`A-${name}@pivot.invalid`], started_at_ms: at(window[0]), ended_at_ms: at(window[1]) })
    )
  }
}

const JOB_LOG = [
  "exit 0",
  "sut  | 2026-10-14T09:00:00.000Z early",
  "sut  | 2026-10-14T09:00:10.000Z run1 cell a",
  "sut  | 2026-10-14T09:00:20.000Z run1 cell b",
  "sut  | 2026-10-14T09:00:40.000Z run2 cell",
  "sut  | 2026-10-14T09:01:00.000Z late",
  ""
].join("\n")

describe("HookKeep.keep", () => {
  it("keeps each run the job's log inside its cells' window, and nothing on a run that states none", async () => {
    const hooks = path.join(root, "_jobs", "7", "hooks")
    fs.mkdirSync(hooks, { recursive: true })
    fs.writeFileSync(path.join(hooks, "sut-logs.log"), JOB_LOG)
    fs.writeFileSync(path.join(hooks, "captures.txt"), "x\n")
    const run1 = path.join(root, "x", "run1")
    const run2 = path.join(root, "y", "run2")
    const fake = path.join(root, "z", "run3")
    cell(run1, "x-a", ["2026-10-14T09:00:10.000Z", "2026-10-14T09:00:12.000Z"])
    cell(run1, "x-b", ["2026-10-14T09:00:18.000Z", "2026-10-14T09:00:19.500Z"])
    cell(run2, "y-a", ["2026-10-14T09:00:39.500Z", "2026-10-14T09:00:41.000Z"])
    cell(fake, "z-a")

    const kept = await Effect.runPromise(HookKeep.keep(hooks, [run1, run2, fake]).pipe(Effect.provide(NodeServices.layer)))
    expect(kept).toBe(2)
    expect(fs.readFileSync(path.join(run1, "hooks", "sut-logs.log"), "utf8")).toBe(
      "exit 0\nsut  | 2026-10-14T09:00:10.000Z run1 cell a\nsut  | 2026-10-14T09:00:20.000Z run1 cell b\n"
    )
    expect(fs.readFileSync(path.join(run2, "hooks", "sut-logs.log"), "utf8")).toBe(
      "exit 0\nsut  | 2026-10-14T09:00:40.000Z run2 cell\n"
    )
    expect(fs.existsSync(path.join(fake, "hooks"))).toBe(false)
  })

  it("shares one cut between runs whose windows overlap, over the union of their windows", async () => {
    const hooks = path.join(root, "_jobs", "8", "hooks")
    fs.mkdirSync(hooks, { recursive: true })
    fs.writeFileSync(path.join(hooks, "sut-logs.log"), JOB_LOG)
    const run1 = path.join(root, "x", "run1")
    const run3 = path.join(root, "w", "run3")
    cell(run1, "x-a", ["2026-10-14T09:00:10.000Z", "2026-10-14T09:00:12.000Z"])
    cell(run3, "w-a", ["2026-10-14T09:00:11.000Z", "2026-10-14T09:00:20.000Z"])

    expect(await Effect.runPromise(HookKeep.keep(hooks, [run1, run3]).pipe(Effect.provide(NodeServices.layer)))).toBe(2)
    const one = path.join(run1, "hooks", "sut-logs.log")
    const three = path.join(run3, "hooks", "sut-logs.log")
    expect(fs.statSync(one).ino).toBe(fs.statSync(three).ino)
    expect(fs.readFileSync(three, "utf8")).toBe(
      "exit 0\nsut  | 2026-10-14T09:00:10.000Z run1 cell a\nsut  | 2026-10-14T09:00:20.000Z run1 cell b\n"
    )
  })

  it("cuts a long log line by line, its window's lines alone kept", async () => {
    const hooks = path.join(root, "_jobs", "9", "hooks")
    fs.mkdirSync(hooks, { recursive: true })
    const log = fs.openSync(path.join(hooks, "big.log"), "w")
    fs.writeSync(log, "exit 0\n")
    const base = Date.parse("2026-10-14T09:00:00.000Z")
    const filler = "y".repeat(200)
    for (let i = 0; i < 50_000; i++) fs.writeSync(log, `sut  | ${new Date(base + i).toISOString()} ${filler}\n`)
    fs.closeSync(log)
    const run = path.join(root, "v", "run9")
    cell(run, "v-a", ["2026-10-14T09:00:20.000Z", "2026-10-14T09:00:21.000Z"])
    expect(await Effect.runPromise(HookKeep.keep(hooks, [run]).pipe(Effect.provide(NodeServices.layer)))).toBe(1)
    // One line a millisecond: the padded window [19 s, 22 s] holds 3001 of them.
    const kept = fs.readFileSync(path.join(run, "hooks", "big.log"), "utf8").trim().split("\n")
    expect(kept[0]).toBe("exit 0")
    expect(kept.length - 1).toBe(3_001)
    expect(kept[1]).toContain("09:00:19.000Z")
    expect(kept.at(-1)).toContain("09:00:22.000Z")
  })
})
