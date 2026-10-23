/**
 * Which census run a cut reads, and what a cut without one is told.
 *
 * The load-bearing claim is the refusal: a cut that read no census would state a
 * different document for every census-charged capture, and the store would book
 * the moves as the code's.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Census from "../../src/cut/census.js"
import { contentHash } from "../../src/fingerprint.js"
import type { Paths } from "../../src/paths.js"

const made: Array<string> = []
afterAll(() => {
  for (const dir of made) fs.rmSync(dir, { recursive: true, force: true })
})

const scratch = (): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "census-"))
  made.push(dir)
  return dir
}

const run = (dir: string, name: string): string => {
  const file = path.join(dir, name)
  fs.writeFileSync(file, "{}\n")
  return file
}

const on = <A, E>(effect: Effect.Effect<A, E, FileSystem.FileSystem>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const pathsAt = (census: string): Paths => ({
  corpus: "/nowhere/corpus",
  sessions: "/nowhere/sessions",
  census,
  registry: "/nowhere/registry.json",
  repoRoot: "/nowhere"
})

describe("the run a cut reads", () => {
  it("is the latest dated report in the census directory", async () => {
    const dir = scratch()
    run(dir, "2026-08-23.census.json")
    const latest = run(dir, "2026-09-05.census.json")
    run(dir, "2026-09-05.census-summary.md")
    // Named outside the dated-run pattern on purpose: a candidate is not a run.
    run(dir, "2026-09-05.rung-byte-identical.candidate.census.json")
    expect(await on(Census.latest(dir))).toBe(latest)
  })

  it("is none where the directory holds no dated run", async () => {
    const dir = scratch()
    run(dir, "README.md")
    expect(await on(Census.latest(dir))).toBeUndefined()
  })

  it("is none where there is no directory at all", async () => {
    expect(await on(Census.latest(path.join(scratch(), "absent")))).toBeUndefined()
  })
})

describe("a cut without one", () => {
  it("is refused, naming where it looked", async () => {
    const census = path.join(scratch(), "absent")
    expect(await on(Census.refusal(pathsAt(census)))).toContain(census)
  })

  it("is not refused where a run is there to read", async () => {
    const census = scratch()
    run(census, "2026-09-05.census.json")
    expect(await on(Census.refusal(pathsAt(census)))).toBeNull()
  })
})

describe("the census coverage of a cut", () => {
  it("counts the captures the run swept and names the others in the cut's order", () => {
    const coverage = Census.captureCoverage(["c", "a", "b", "d"], ["a", "b", "z"])
    expect(coverage).toEqual({ covered: 2, total: 4, uncovered: ["c", "d"] })
    expect(Census.coverageLine(coverage)).toBe("census covers 2/4 captures; uncovered: c, d")
  })

  it("says only the count where every capture is covered", () => {
    expect(Census.coverageLine(Census.captureCoverage(["a"], ["a"]))).toBe("census covers 1/1 captures")
  })

  it("names the first ten uncovered and counts the rest", () => {
    const captures = Array.from({ length: 13 }, (_, i) => `cap${i}`)
    const line = Census.coverageLine(Census.captureCoverage(captures, []))
    expect(line).toContain("census covers 0/13 captures; uncovered: cap0, cap1")
    expect(line).toContain("cap9 (+3 more)")
    expect(line).not.toContain("cap10")
  })
})

describe("the captures a census run swept", () => {
  it("are read off the head of a report that lists them before its hits", async () => {
    const file = path.join(scratch(), "2026-10-12.census.json")
    const hits = Array.from({ length: 3 }, (_, i) => ({ capture: `hit-only-${i}`, note: "a ] in a [string" }))
    fs.writeFileSync(file, JSON.stringify({ documents: 2, captures: ["a]b", "c"], groups: 0, hits }, null, 2))
    expect(await on(Census.sweptCaptures(file))).toEqual(["a]b", "c"])
  })

  it("are read off the whole report where its hits come first", async () => {
    const file = path.join(scratch(), "2026-10-12.census.json")
    fs.writeFileSync(file, JSON.stringify({ hits: [{ captures: ["decoy"] }], captures: ["c"] }))
    expect(await on(Census.sweptCaptures(file))).toEqual(["c"])
  })

  it("are none where the report states no list", async () => {
    const file = path.join(scratch(), "2026-10-12.census.json")
    fs.writeFileSync(file, JSON.stringify({ documents: 0, hits: [] }))
    expect(await on(Census.sweptCaptures(file))).toEqual([])
  })

  it("cannot be read off an empty report, which a cut is refused on by name", async () => {
    const dir = scratch()
    const file = path.join(dir, "2026-10-12.census.json")
    fs.writeFileSync(file, "")
    expect(await on(Census.sweptCaptures(file))).toEqual({ unreadable: expect.stringContaining("is empty") })
    expect(await on(Census.refusal(pathsAt(dir)))).toContain("is empty")
  })
})

describe("the census run in the cut fingerprint", () => {
  it("rides under its own file name, so two hosts holding the same run agree", async () => {
    const here = run(scratch(), "2026-09-05.census.json")
    const there = run(scratch(), "2026-09-05.census.json")
    const name = `census/${path.basename(here)}`
    expect(await on(contentHash("/", [], [{ name, file: here }]))).toBe(
      await on(contentHash("/", [], [{ name, file: there }]))
    )
  })

  it("moves the fingerprint when the run's content moves", async () => {
    const dir = scratch()
    const file = run(dir, "2026-09-05.census.json")
    const name = `census/${path.basename(file)}`
    const before = await on(contentHash("/", [], [{ name, file }]))
    fs.writeFileSync(file, "{\"hits\":[]}\n")
    expect(await on(contentHash("/", [], [{ name, file }]))).not.toBe(before)
  })
})
