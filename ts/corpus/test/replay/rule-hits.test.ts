/**
 * One finished bundle folded into rule-hit rows: a cell's own `rule-hits.json`
 * is read as written; a cell that wrote none is read off its confrontation
 * sheet through the deployment's `CellHost.hitsOf`, told the pass the run's
 * index states over its verdict's; a cell whose counts will not decode is
 * left out of the cells read; and each row counts hits, the cells they landed
 * on, and one cell by name.
 */
import { NodeFileSystem } from "@effect/platform-node"
import { CellHits, Confrontation, E2e } from "@sip/contracts"
import { Layout } from "@sip/driver"
import type { Reclassifier } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { CellHost } from "../../src/deployment/index.js"
import { countBundle } from "../../src/replay/rule-hits.js"

const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "corpus-rule-hits-"))
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }))

const LANE = "paused"

/** What `hitsOf` was asked, by cell. */
const asked = new Map<string, boolean>()

/** A cell host that counts one `sheet` hit per record, bucketed by whether the cell passed. */
const host = CellHost.layerWith({
  tests: Effect.die("unused"),
  services: () => Layer.empty as never,
  auditLines: () => Effect.succeed([]),
  charges: (_dir, findings) => Effect.succeed(findings),
  hitsOf: (outcome: Reclassifier.RunOutcome, records) => {
    asked.set(outcome.caseId, outcome.passed)
    return records.map((one) => ({
      family: "sheet",
      rule: one.rule,
      bucket: outcome.passed ? "passed" : "failed",
      hits: 1
    }))
  }
})

const count = (runDir: string) =>
  Effect.runPromise(countBundle(runDir, LANE).pipe(Effect.provide(Layer.merge(host, NodeFileSystem.layer))))

const record = (rule: string): Confrontation.ConfrontationRecord => ({
  lane: LANE,
  capture: "c.pcap.gz",
  case: "c",
  run: 0,
  step: "s1",
  kind: "header",
  signature: "header:via:response:100:INVITE",
  name: "Via",
  scope: "response:100:INVITE",
  captured: ["a"],
  replayed: ["b"],
  inbound: false,
  added: [],
  removed: [],
  class: "accepted",
  rule,
  ticket: ""
})

/** One cell: its sheet always, its own counts and verdict where given. */
const cell = (
  runDir: string,
  name: string,
  given: { readonly hits?: string; readonly verdict?: "ok" | "failed"; readonly rules?: ReadonlyArray<string> }
): void => {
  const dir = path.join(runDir, name)
  fs.mkdirSync(dir, { recursive: true })
  const lines = (given.rules ?? []).map((rule) => Confrontation.emitConfrontationRecord(record(rule)))
  fs.writeFileSync(path.join(dir, Layout.CONFRONTATION_FILE), lines.map((line) => `${line}\n`).join(""))
  if (given.hits !== undefined) fs.writeFileSync(path.join(dir, Layout.RULE_HITS_FILE), given.hits)
  if (given.verdict !== undefined) {
    fs.writeFileSync(path.join(dir, Layout.VERDICT_FILE), JSON.stringify({ case: name, lane: LANE, status: given.verdict }))
  }
}

describe("countBundle", () => {
  it("folds each cell's counts into rows: written counts as written, the rest off the sheet", async () => {
    const runDir = path.join(ROOT, "run1")
    cell(runDir, "a-written", {
      hits: CellHits.emitHits([{ family: "tolerance", rule: "t1", bucket: "header", hits: 2 }]),
      rules: ["ignored"]
    })
    cell(runDir, "b-sheet", { verdict: "ok", rules: ["r1", "r1"] })
    cell(runDir, "c-sheet", { verdict: "ok", rules: ["r1"] })
    cell(runDir, "d-unverdicted", { rules: ["r2"] })
    cell(runDir, "e-broken", { hits: "{not json", rules: ["r3"] })
    fs.writeFileSync(path.join(runDir, "no-sheet.txt"), "")
    const summary = (dir: string, passed: boolean) => ({
      cell: { case: dir, shape: "pivot-replay", infra: LANE },
      passed,
      dir
    })
    fs.writeFileSync(
      path.join(runDir, Layout.CAMPAIGN_INDEX),
      E2e.emitCampaignIndex({ campaign: "run1", ts: "t", cells: [summary("b-sheet", true), summary("c-sheet", false)] })
    )
    asked.clear()
    const counted = await count(runDir)
    expect(counted.cells).toBe(4)
    // The index's gate over the verdict: a panicked run fails whatever verdict it left.
    expect(asked.get("b-sheet")).toBe(true)
    expect(asked.get("c-sheet")).toBe(false)
    const rows = [...counted.hits].sort((a, b) => `${a.rule}${a.bucket}`.localeCompare(`${b.rule}${b.bucket}`))
    expect(rows.find((row) => row.rule === "t1")).toEqual({
      family: "tolerance",
      rule: "t1",
      bucket: "header",
      hits: 2,
      cells: 1,
      sample: "a-written"
    })
    expect(rows.find((row) => row.rule === "r1" && row.bucket === "passed")).toMatchObject({ hits: 2, cells: 1, sample: "b-sheet" })
    expect(rows.find((row) => row.rule === "r1" && row.bucket === "failed")).toMatchObject({ hits: 1, cells: 1, sample: "c-sheet" })
    expect(rows.find((row) => row.rule === "r2")).toMatchObject({ bucket: "failed", hits: 1, cells: 1, sample: "d-unverdicted" })
    expect(rows.some((row) => row.rule === "r3" || row.rule === "ignored")).toBe(false)
    expect(asked.get("d-unverdicted")).toBe(false)
    expect(asked.has("a-written")).toBe(false)
  })
})
