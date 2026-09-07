/**
 * A whole campaign, end to end against stub binaries: both cell shapes into one
 * run dir, one `campaign.json`, and an exit code that is the verdict.
 */
import { Campaign, CellHits, E2e } from "@sip/contracts"
import { Reclassifier } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { exitCodeOf, runCampaign, summarize, type CampaignRun } from "../src/campaign.js"
import { CAMPAIGN_INDEX, ERROR_FILE, RULE_HITS_FILE, SKIP_FILE, SPECS_DIR } from "../src/layout.js"
import { LanePresets, LaneUnknown } from "../src/lanes.js"
import { caseNamed, caseWithLanes, pivotDocument, rig, runDir, stubTests, type RigOptions } from "./harness.js"

const CASE = pivotDocument("transparent-defect.v3.json")

const dirs: Array<string> = []
afterEach(() => {
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

const replayCell = (over: Partial<Campaign.PivotReplayCell> = {}): Campaign.PivotReplayCell => ({
  kind: "pivot-replay",
  case: CASE,
  lane: "stub-lane",
  ...over
})

const rustCell = (name = "bc_02::transparent"): Campaign.RustTestCell => ({
  kind: "rust-test",
  crate: "stub-crate",
  name
})

const campaign = async (
  cells: ReadonlyArray<Campaign.CampaignCell>,
  options: { tests?: ReturnType<typeof stubTests>; rig?: RigOptions } = {}
): Promise<CampaignRun & { readonly dir: string }> => {
  const dir = runDir("campaign")
  dirs.push(dir)
  const run = await Effect.runPromise(
    runCampaign(
      { campaign: "stub", cells },
      { runDir: dir, ts: "2026-08-25T00:00:00Z", tests: options.tests ?? stubTests(0, true) }
    ).pipe(Effect.provide(rig(options.rig ?? {})))
  )
  return { ...run, dir }
}

describe("a mixed campaign", () => {
  it("runs both shapes into one run dir and folds one index", async () => {
    const run = await campaign([rustCell(), replayCell()])
    expect(run.index.campaign).toBe("stub")
    expect(run.index.ts).toBe("2026-08-25T00:00:00Z")
    expect(run.index.cells.map((c) => c.cell.shape)).toEqual(["rust-test", "pivot-replay"])
    expect(E2e.campaignPassed(run.index)).toBe(true)
    expect(exitCodeOf(run)).toBe(0)
  })

  it("writes the index where a reader looks for it, in the e2e byte form", async () => {
    const run = await campaign([replayCell()])
    expect(run.indexPath).toBe(path.join(run.dir, CAMPAIGN_INDEX))
    const text = fs.readFileSync(run.indexPath, "utf8")
    expect(text).toBe(E2e.emitCampaignIndex(run.index))
    expect(E2e.decodeCampaignIndexSync(JSON.parse(text) as unknown)).toEqual(run.index)
  })

  it("keeps the run-spec clear of the directory the interpreter wipes", async () => {
    const run = await campaign([replayCell()])
    const cell = run.index.cells[0]!
    // The stub removes its `out_dir` before writing, exactly as the interpreter
    // does — so a spec written there would not survive the run.
    expect(fs.existsSync(path.join(run.dir, cell.dir, "verdict.json"))).toBe(true)
    expect(fs.readdirSync(path.join(run.dir, SPECS_DIR))).toEqual([`${cell.dir}.run-spec.json`])
  })

  it("hands the interpreter the cell directory as its own out_dir", async () => {
    const run = await campaign([replayCell()])
    const cell = run.index.cells[0]!
    const spec = JSON.parse(
      fs.readFileSync(path.join(run.dir, SPECS_DIR, `${cell.dir}.run-spec.json`), "utf8")
    ) as { out_dir: string; lane: { kind: string }; run: unknown }
    expect(spec.out_dir).toBe(path.join(run.dir, cell.dir))
    expect(spec.lane.kind).toBe("stub")
    expect(spec.run).toEqual({ timing_tolerance_ms: 0 })
  })
})

describe("a red cell", () => {
  it("is red, not crashed, when the run produced a failing verdict", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([replayCell({ case: caseNamed(cases, "fails.v3.json") })])
    const cell = run.index.cells[0]!
    // A failing run still left a bundle, so it is a RED cell and not a crashed
    // one: the difference is whether anyone can read what happened.
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
    expect(fs.existsSync(path.join(run.dir, cell.dir, "verdict.json"))).toBe(true)
    expect(exitCodeOf(run)).toBe(1)
  })

  it("IS crashed where the interpreter refused before writing anything", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([replayCell({ case: caseNamed(cases, "no-bundle.v3.json") })])
    const cell = run.index.cells[0]!
    expect(cell.passed).toBe(false)
    expect(cell.error).toContain("left no bundle")
  })

  it("fails the campaign and the exit code with it", async () => {
    const run = await campaign([rustCell()], { tests: stubTests(101, true) })
    expect(run.index.cells[0]!.passed).toBe(false)
    expect(run.index.cells[0]!.error).toBeUndefined()
    expect(exitCodeOf(run)).toBe(1)
    expect(summarize(run)).toContain("FAILED")
  })
})

describe("a lane the document declares blocked", () => {
  it("is skipped, not failed, and the interpreter is never invoked", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "stub-lane": "blocked:claim-ambiguous" }) })
    ])
    const cell = run.index.cells[0]!
    expect(cell.skipped).toBe("blocked:claim-ambiguous")
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
    // Nothing ran: no spec was written and the stub left no bundle behind.
    expect(fs.existsSync(path.join(run.dir, SPECS_DIR))).toBe(false)
    expect(fs.existsSync(path.join(run.dir, cell.dir, "verdict.json"))).toBe(false)
    expect(E2e.campaignPassed(run.index)).toBe(true)
    expect(exitCodeOf(run)).toBe(0)
    expect(summarize(run)).toContain("SKIP")
  })

  it("leaves a skipped.json saying which lane and why, verbatim", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "stub-lane": "blocked:multi-party-not-driveable" }) })
    ])
    const cell = run.index.cells[0]!
    const text = fs.readFileSync(path.join(run.dir, cell.dir, SKIP_FILE), "utf8")
    const skip = E2e.decodeCellSkipSync(JSON.parse(text) as unknown)
    expect(skip.verdict).toBe("blocked:multi-party-not-driveable")
    expect(skip.cell.infra).toBe("stub-lane")
    expect(text).toBe(E2e.emitCellSkip(skip))
  })

  it("runs the cell where the document says the lane is ok", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([
      replayCell({
        case: caseWithLanes(cases, "ok.v3.json", { "stub-lane": "ok", "other-lane": "blocked:whatever" })
      })
    ])
    const cell = run.index.cells[0]!
    expect(cell.skipped).toBeUndefined()
    expect(cell.passed).toBe(true)
  })

  it("runs the cell where the document does not mention the lane at all", async () => {
    // Absence means "none" — a lane nobody ruled on is not a blocked lane.
    const run = await campaign([replayCell()])
    expect(run.index.cells[0]!.skipped).toBeUndefined()
    expect(run.index.cells[0]!.passed).toBe(true)
  })

  it("never covers for a cell beside it that did run and failed", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const run = await campaign([
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "stub-lane": "blocked:claim-ambiguous" }) }),
      replayCell({ case: caseNamed(cases, "fails.v3.json") })
    ])
    expect(run.index.cells[0]!.skipped).toBe("blocked:claim-ambiguous")
    expect(run.index.cells[1]!.passed).toBe(false)
    expect(exitCodeOf(run)).toBe(1)
  })
})

describe("a cell that never produced a result", () => {
  it("leaves an error.txt and says so in the index", async () => {
    const run = await campaign([rustCell()], { tests: stubTests(101, false) })
    const cell = run.index.cells[0]!
    expect(cell.passed).toBe(false)
    expect(cell.error).toContain("exited 101")
    expect(fs.readFileSync(path.join(run.dir, cell.dir, ERROR_FILE), "utf8")).toContain("stub stderr")
    expect(summarize(run)).toContain("ERROR")
  })

  it("does not take the rest of the campaign down with it", async () => {
    const run = await campaign([rustCell("crashes"), replayCell()], {
      tests: {
        command: "/nonexistent/runner",
        args: () => [],
        cellDirEnv: "CELL_DIR"
      }
    })
    expect(run.index.cells[0]!.error).toBeDefined()
    expect(run.index.cells[1]!.passed).toBe(true)
    expect(fs.existsSync(run.indexPath)).toBe(true)
  })

  it("is what an unknown lane produces, because a lane is never guessed", async () => {
    const run = await campaign([replayCell({ lane: "nobody-described-this" })], {
      rig: { lanes: LanePresets.layer }
    })
    const cell = run.index.cells[0]!
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeDefined()
    const written = fs.readFileSync(path.join(run.dir, cell.dir, ERROR_FILE), "utf8")
    expect(written).toContain("LaneUnknown")
    expect(new LaneUnknown({ lane: "x", detail: "y" })._tag).toBe("Driver.LaneUnknown")
  })
})

describe("the post-run escape hatch", () => {
  it("lets a deployment restate a cell, and the reason rides the run", async () => {
    const run = await campaign([replayCell()], {
      rig: {
        reclassifier: Reclassifier.layerWith((outcome) =>
          Effect.succeed({ passed: false, reason: `${outcome.lane} is a known open defect` })
        )
      }
    })
    expect(run.index.cells[0]!.passed).toBe(false)
    expect(run.cells[0]!.detail).toContain("known open defect")
    expect(exitCodeOf(run)).toBe(1)
  })

  it("changes nothing by default", async () => {
    const run = await campaign([replayCell()], { rig: {} })
    expect(run.index.cells[0]!.passed).toBe(true)
    expect(fs.existsSync(path.join(run.dir, run.index.cells[0]!.dir, RULE_HITS_FILE))).toBe(false)
  })

  it("writes the deployment's per-cell counts beside the cell, in its own tokens", async () => {
    const hits: CellHits.Hits = [{ family: "tolerance", rule: "via-stack-owned", bucket: "accepted", hits: 2 }]
    const run = await campaign([replayCell()], {
      rig: { reclassifier: Reclassifier.layerWith((outcome) => Effect.succeed({ passed: outcome.passed, hits })) }
    })
    const written = fs.readFileSync(path.join(run.dir, run.index.cells[0]!.dir, RULE_HITS_FILE), "utf8")
    expect(CellHits.decodeHitsSync(JSON.parse(written) as unknown)).toEqual(hits)
  })
})

/** The stub layer is a Layer, so it composes like any deployment's would. */
describe("composition", () => {
  it("is what a deployment substitutes, and nothing else", () => {
    const composed = LanePresets.layerWith(() => Effect.succeed({ kind: "whatever" }))
    expect(Layer.isLayer(composed)).toBe(true)
  })
})
