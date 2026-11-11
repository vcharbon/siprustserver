/**
 * One cell, end to end against stub binaries: either shape lands in the run
 * directory it is handed, and its summary is the verdict a reader is told.
 */
import { Campaign, CellHits, Confrontation, E2e } from "@sip/contracts"
import { Reclassifier } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { type CellRun, runCell } from "../src/cells.js"
import { CONFRONTATION_FILE, ERROR_FILE, RULE_HITS_FILE, SKIP_FILE, SPECS_DIR, STDERR_FILE } from "../src/layout.js"
import { LanePresets, LaneUnknown } from "../src/lanes.js"
import { RoutingCompiler } from "../src/routing.js"
import {
  caseExpectingBody,
  caseExpectingSdp,
  caseNamed,
  caseWithLanes,
  pivotDocument,
  rig,
  runDir,
  stubTests,
  type RigOptions
} from "./harness.js"

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

const rustCell = (name = "relay::transparent"): Campaign.RustTestCell => ({
  kind: "rust-test",
  crate: "stub-crate",
  name
})

interface Options {
  readonly tests?: ReturnType<typeof stubTests>
  readonly rig?: RigOptions
}

/** Every cell of `list`, one after the other, into one fresh run directory. */
const cells = async (
  list: ReadonlyArray<Campaign.CampaignCell>,
  options: Options = {}
): Promise<{ readonly dir: string; readonly runs: ReadonlyArray<CellRun> }> => {
  const dir = runDir("cells")
  dirs.push(dir)
  const context = { runDir: dir, tests: options.tests ?? stubTests(0, true), mode: "record" as const }
  const runs = await Effect.runPromise(
    Effect.forEach(list, (cell) => runCell(cell, context)).pipe(Effect.provide(rig(options.rig ?? {})))
  )
  return { dir, runs }
}

/** One cell into a fresh run directory: its summary, its detail and where it ran. */
const one = async (
  cell: Campaign.CampaignCell,
  options: Options = {}
): Promise<{ readonly dir: string; readonly cell: E2e.CellSummary; readonly detail: string }> => {
  const { dir, runs } = await cells([cell], options)
  return { dir, cell: runs[0]!.summary, detail: runs[0]!.detail }
}

describe("a cell of either shape", () => {
  it("runs into the run dir it is handed, under its own cell id", async () => {
    const { runs } = await cells([rustCell(), replayCell()])
    expect(runs.map((r) => r.summary.cell.shape)).toEqual(["rust-test", "pivot-replay"])
    expect(runs.every((r) => r.summary.passed)).toBe(true)
  })

  it("keeps the run-spec clear of the directory the interpreter wipes", async () => {
    const { dir, cell } = await one(replayCell())
    // The stub removes its `out_dir` before writing, exactly as the interpreter
    // does — so a spec written there would not survive the run.
    expect(fs.existsSync(path.join(dir, cell.dir, "verdict.json"))).toBe(true)
    expect(fs.readdirSync(path.join(dir, SPECS_DIR))).toEqual([`${cell.dir}.run-spec.json`])
  })

  it("hands the interpreter the cell directory as its own out_dir", async () => {
    const { dir, cell } = await one(replayCell())
    const spec = JSON.parse(
      fs.readFileSync(path.join(dir, SPECS_DIR, `${cell.dir}.run-spec.json`), "utf8")
    ) as { out_dir: string; lane: { kind: string }; run: unknown }
    expect(spec.out_dir).toBe(path.join(dir, cell.dir))
    expect(spec.lane.kind).toBe("stub")
    expect(spec.run).toEqual({ timing_tolerance_ms: 0 })
  })
})

describe("a red cell", () => {
  it("is red, not crashed, when the run produced a failing verdict", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, cell } = await one(replayCell({ case: caseNamed(cases, "fails.v3.json") }))
    // A failing run still left a bundle, so it is a RED cell and not a crashed
    // one: the difference is whether anyone can read what happened.
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
    expect(fs.existsSync(path.join(dir, cell.dir, "verdict.json"))).toBe(true)
  })

  it("is red where the run panicked, whatever verdict it left", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, cell } = await one(replayCell({ case: caseNamed(cases, "panics.v3.json") }))
    const verdict = JSON.parse(fs.readFileSync(path.join(dir, cell.dir, "verdict.json"), "utf8"))
    expect(verdict.status).toBe("ok")
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
  })

  it("keeps its interpreter's stderr, failing verdict or panic alike, and a green cell keeps none", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, runs } = await cells([
      replayCell({ case: caseNamed(cases, "fails.v3.json") }),
      replayCell({ case: caseNamed(cases, "panics.v3.json") }),
      replayCell()
    ])
    const [failed, panicked, green] = runs.map((r) => r.summary)
    expect(fs.readFileSync(path.join(dir, failed!.dir, STDERR_FILE), "utf8")).toBe("the run failed\n")
    expect(fs.readFileSync(path.join(dir, panicked!.dir, STDERR_FILE), "utf8")).toBe("the run body panicked\n")
    expect(green!.passed).toBe(true)
    expect(fs.existsSync(path.join(dir, green!.dir, STDERR_FILE))).toBe(false)
    // A red cell is not a crashed one: it leaves no error file.
    expect(fs.existsSync(path.join(dir, failed!.dir, ERROR_FILE))).toBe(false)
  })

  it("is not passed where its lane still waits on one of its lines, whatever verdict it left", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, cell, detail } = await one(replayCell({ case: caseNamed(cases, "pending-line.v3.json") }))
    const verdict = JSON.parse(fs.readFileSync(path.join(dir, cell.dir, "verdict.json"), "utf8"))
    expect(verdict.status).toBe("ok")
    expect(cell.passed).toBe(false)
    expect(cell.pending).toEqual(["cleanup"])
    expect(cell.error).toBeUndefined()
    expect(detail).toContain("still verifying: cleanup pending")
  })

  it("IS crashed where the interpreter refused before writing anything", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(replayCell({ case: caseNamed(cases, "no-bundle.v3.json") }))
    expect(cell.passed).toBe(false)
    expect(cell.error).toContain("left no bundle")
  })

  it("is red, not crashed, where the crate test failed and its reporter wrote a result", async () => {
    const { cell } = await one(rustCell(), { tests: stubTests(101, true) })
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
  })
})

describe("an expected body", () => {
  it("is read from the case directory and confronted with the reception", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const stored = "<request><play/></request>"
    const { dir, cell } = await one(replayCell({ case: caseExpectingBody(cases, stored) }))
    expect(cell.error).toBeUndefined()
    const records = Confrontation.parseConfrontationLines(
      fs.readFileSync(path.join(dir, cell.dir, CONFRONTATION_FILE), "utf8")
    )
    expect(records.filter((r) => r.kind === "body")).toMatchObject([
      {
        step: "s9",
        signature: "body:application/example+xml:request:INFO:in-dialog",
        name: "application/example+xml",
        captured: [stored],
        replayed: ["<other/>"]
      }
    ])
  })

  it("compared as a session description names the differing media line, lane-owned fields aside", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const stored =
      "v=0\r\no=- 1 2 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\n" +
      "m=audio 6000 RTP/AVP 8\r\na=sendrecv\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\n"
    const { dir, cell } = await one(replayCell({ case: caseExpectingSdp(cases, stored) }))
    expect(cell.error).toBeUndefined()
    const records = Confrontation.parseConfrontationLines(
      fs.readFileSync(path.join(dir, cell.dir, CONFRONTATION_FILE), "utf8")
    )
    // The stub ran verbatim, so the `o=` numbers and the `c=` address it
    // changed are rows beside the codec; the attribute order is not.
    expect(records.filter((r) => r.kind === "body").map((r) => [r.signature, r.captured, r.replayed])).toEqual([
      ["body:sdp:session:o=:request:INFO:in-dialog", ["o=- 1 2 IN IP4 10.0.0.1"], ["o=- 7 8 IN IP4 10.0.0.1"]],
      ["body:sdp:session:c=:request:INFO:in-dialog", ["c=IN IP4 192.0.2.10"], ["c=IN IP4 10.0.0.1"]],
      ["body:sdp:m0:m=:request:INFO:in-dialog", ["m=audio 6000 RTP/AVP 8"], ["m=audio 4000 RTP/AVP 0"]],
      ["body:sdp:m0:a=rtpmap:request:INFO:in-dialog", ["a=rtpmap:8 PCMA/8000"], ["a=rtpmap:0 PCMU/8000"]]
    ])
    expect(records.filter((r) => r.kind === "body").every((r) => r.name === "application/sdp")).toBe(true)
  })
})

describe("a lane the document declares blocked", () => {
  it("is skipped, not failed, and the interpreter is never invoked", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, cell } = await one(
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "stub-lane": "blocked:claim-ambiguous" }) })
    )
    expect(cell.skipped).toBe("blocked:claim-ambiguous")
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeUndefined()
    // Nothing ran: no spec was written and the stub left no bundle behind.
    expect(fs.existsSync(path.join(dir, SPECS_DIR))).toBe(false)
    expect(fs.existsSync(path.join(dir, cell.dir, "verdict.json"))).toBe(false)
  })

  it("leaves a skipped.json saying which lane and why, verbatim", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { dir, cell } = await one(
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "stub-lane": "blocked:multi-party-not-driveable" }) })
    )
    const text = fs.readFileSync(path.join(dir, cell.dir, SKIP_FILE), "utf8")
    const skip = E2e.decodeCellSkipSync(JSON.parse(text) as unknown)
    expect(skip.verdict).toBe("blocked:multi-party-not-driveable")
    expect(skip.cell.infra).toBe("stub-lane")
    expect(text).toBe(E2e.emitCellSkip(skip))
  })

  it("runs the cell where the document says the lane is ok", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(
      replayCell({
        case: caseWithLanes(cases, "ok.v3.json", { "stub-lane": "ok", "other-lane": "blocked:whatever" })
      })
    )
    expect(cell.skipped).toBeUndefined()
    expect(cell.passed).toBe(true)
  })

  it("is skipped on the verdict stated for every lane where the document does not name it", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(
      replayCell({ case: caseWithLanes(cases, "blocked.v3.json", { "other-lane": "ok", "*": "blocked:claim-ambiguous" }) })
    )
    expect(cell.skipped).toBe("blocked:claim-ambiguous")
  })

  it("runs the cell on its own ok verdict, whatever the document states for every lane", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(
      replayCell({ case: caseWithLanes(cases, "ok.v3.json", { "stub-lane": "ok", "*": "blocked:whatever" }) })
    )
    expect(cell.skipped).toBeUndefined()
    expect(cell.passed).toBe(true)
  })

  it("runs the cell where the document does not mention the lane at all", async () => {
    // Absence means "none" — a lane nobody ruled on is not a blocked lane.
    const { cell } = await one(replayCell())
    expect(cell.skipped).toBeUndefined()
    expect(cell.passed).toBe(true)
  })
})

describe("a cell that never produced a result", () => {
  it("leaves an error.txt and says so in its summary", async () => {
    const { dir, cell } = await one(rustCell(), { tests: stubTests(101, false) })
    expect(cell.passed).toBe(false)
    expect(cell.error).toContain("exited 101")
    expect(fs.readFileSync(path.join(dir, cell.dir, ERROR_FILE), "utf8")).toContain("stub stderr")
  })

  it("answers its summary rather than failing, so the cell after it still runs", async () => {
    const { runs } = await cells([rustCell("crashes"), replayCell()], {
      tests: {
        command: "/nonexistent/runner",
        args: () => [],
        cellDirEnv: "CELL_DIR"
      }
    })
    expect(runs[0]!.summary.error).toBeDefined()
    expect(runs[1]!.summary.passed).toBe(true)
  })

  it("carries the host's own words where it runs no crate test, and spawns nothing", async () => {
    const { dir, runs } = await cells([rustCell(), replayCell()], {
      tests: {
        command: "/nonexistent/runner",
        args: () => [],
        cellDirEnv: "CELL_DIR",
        unavailable: "crate-test cells: not available on this host"
      }
    })
    const cell = runs[0]!.summary
    expect(cell.passed).toBe(false)
    expect(cell.error).toBe("crate-test cells: not available on this host")
    expect(fs.readFileSync(path.join(dir, cell.dir, ERROR_FILE), "utf8")).toBe(
      "crate-test cells: not available on this host\n"
    )
    expect(runs[1]!.summary.passed).toBe(true)
  })

  it("is what an unknown lane produces, because a lane is never guessed", async () => {
    const { dir, cell } = await one(replayCell({ lane: "nobody-described-this" }), {
      rig: { lanes: LanePresets.layer }
    })
    expect(cell.passed).toBe(false)
    expect(cell.error).toBeDefined()
    const written = fs.readFileSync(path.join(dir, cell.dir, ERROR_FILE), "utf8")
    expect(written).toContain("LaneUnknown")
    expect(written).toContain("nobody-described-this")
    expect(new LaneUnknown({ lane: "x", detail: "y" })._tag).toBe("Driver.LaneUnknown")
  })

  /** A refusal's reason is its message: the error file carries it, not the tag alone. */
  it("names the reason a deployment's routing compiler refused the cell", async () => {
    const { dir, cell } = await one(replayCell(), {
      rig: {
        routing: RoutingCompiler.layerWith((cell, pivot) =>
          Effect.fail(
            new RoutingCompiler.RoutingRefused({
              case: pivot.case.id,
              lane: cell.lane,
              detail: "attempt 1/0 dials a number whose route this compiler does not state"
            })
          )
        )
      }
    })
    expect(cell.passed).toBe(false)
    const written = fs.readFileSync(path.join(dir, cell.dir, ERROR_FILE), "utf8")
    expect(written).toContain("RoutingRefused")
    expect(written).toContain("stub-lane: attempt 1/0 dials a number whose route this compiler does not state")
  })
})

describe("the post-run RFC audit", () => {
  const audited = async (name: string) => {
    const cases = runDir("cases")
    dirs.push(cases)
    return one(replayCell({ case: caseNamed(cases, `${name}.v3.json`) }))
  }

  it("passes a cell whose only finding is a scripted party's the reference shows", async () => {
    expect((await audited("rfc-party-shown")).cell.passed).toBe(true)
  })

  it("passes a cell whose only finding is a scripted party's the document could not judge", async () => {
    expect((await audited("rfc-party-unjudged")).cell.passed).toBe(true)
  })

  it("passes a cell whose only finding is a scripted party's the reference does not show, marked as a source fault", async () => {
    const { cell, detail } = await audited("rfc-source-fault")
    expect(cell.passed).toBe(true)
    expect(cell.sourceFault).toBe(true)
    expect(detail).toContain("a source fault stands as information")
    expect(detail).toContain("repeats a captured message where, in this run, the RFC no longer allows it")
    expect(detail).toContain("no-200-after-cancel [127.0.0.1:5060] by uas1")
    expect(detail).not.toContain("the RFC audit failed the cell")
  })

  it("never lets a source fault hide the SUT's own failure: the cell fails and the detail names both", async () => {
    const { cell, detail } = await audited("rfc-source-fault-beside-a-failure")
    expect(cell.passed).toBe(false)
    expect(cell.sourceFault).toBe(true)
    expect(detail).toContain("the run failed")
    expect(detail).toContain("beside it, a source fault")
    expect(detail).toContain("no-200-after-cancel [127.0.0.1:5060] by uas1")
  })

  it("judges the SUT's side alone: a bless of the SUT's side passes the cell over its source fault", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(replayCell({ case: caseNamed(cases, "rfc-source-fault-beside-a-failure.v3.json") }), {
      rig: {
        reclassifier: Reclassifier.layerWith((outcome) =>
          Effect.succeed({ passed: true, reason: `${outcome.lane}: every failure is a named difference` })
        )
      }
    })
    expect(cell.passed).toBe(true)
    expect(cell.sourceFault).toBe(true)
  })

  it("fails the SUT's own finding as the RFC audit's", async () => {
    const { cell, detail } = await audited("rfc-sut-charged")
    expect(cell.passed).toBe(false)
    expect(detail).toContain("the RFC audit failed the cell")
    expect(detail).toContain("cseq-in-dialog-order")
  })
})

describe("the post-run escape hatch", () => {
  it("lets a deployment restate a cell, and the reason rides the run", async () => {
    const { cell, detail } = await one(replayCell(), {
      rig: {
        reclassifier: Reclassifier.layerWith((outcome) =>
          Effect.succeed({ passed: false, reason: `${outcome.lane} is a known open defect` })
        )
      }
    })
    expect(cell.passed).toBe(false)
    expect(detail).toContain("known open defect")
    expect(cell.restated).toBeUndefined()
  })

  it("names its reason in the rollup where it restates a failed run as passed", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(replayCell({ case: caseNamed(cases, "rfc-source-fault-beside-a-failure.v3.json") }), {
      rig: { reclassifier: Reclassifier.layerWith(() => Effect.succeed({ passed: true, reason: "rule-a — ok with accepted delta" })) }
    })
    expect(cell.passed).toBe(true)
    expect(cell.restated).toBe("rule-a — ok with accepted delta")
  })

  it("never restates a panicked run as passed", async () => {
    const cases = runDir("cases")
    dirs.push(cases)
    const { cell } = await one(replayCell({ case: caseNamed(cases, "panics.v3.json") }), {
      rig: { reclassifier: Reclassifier.layerWith(() => Effect.succeed({ passed: true, reason: "restated" })) }
    })
    expect(cell.passed).toBe(false)
    expect(cell.restated).toBeUndefined()
  })

  it("changes nothing by default", async () => {
    const { dir, cell } = await one(replayCell(), { rig: {} })
    expect(cell.passed).toBe(true)
    expect(cell.restated).toBeUndefined()
    expect(fs.existsSync(path.join(dir, cell.dir, RULE_HITS_FILE))).toBe(false)
  })

  it("writes the deployment's per-cell counts beside the cell, in its own tokens", async () => {
    const hits: CellHits.Hits = [{ family: "tolerance", rule: "via-stack-owned", bucket: "accepted", hits: 2 }]
    const { dir, cell } = await one(replayCell(), {
      rig: { reclassifier: Reclassifier.layerWith((outcome) => Effect.succeed({ passed: outcome.passed, hits })) }
    })
    const written = fs.readFileSync(path.join(dir, cell.dir, RULE_HITS_FILE), "utf8")
    expect(CellHits.decodeHitsSync(JSON.parse(written) as unknown)).toEqual(hits)
  })
})
