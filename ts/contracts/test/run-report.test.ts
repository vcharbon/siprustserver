/**
 * The run report format: one run set's calls, each as the compare report's own
 * cell, read off the run rows or off the summaries a citation kept; the
 * published schema is the one the format generates.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import type * as CompareReport from "../src/compare-report.js"
import * as RunReport from "../src/run-report.js"
import { PUBLISHED } from "../src/schemas.js"

const cell: CompareReport.Cell = {
  run: 9,
  lane: "fake",
  env: "main-checkout",
  branch: "main",
  code: [{ name: "app", sha: "a".repeat(40) }],
  verdict: "failed",
  passed: false,
  repeats: { runs: 1, passed: 0 },
  skipped: null,
  failedStep: "s6",
  mainError: { kind: "died", rule: "expect-timed-out", rfc: null, step: "s6", expected: "200 to INVITE", detail: "" },
  bless: "unmeasured",
  accepted: null,
  knownBugs: null,
  ladder: null,
  links: { study: "http://viewer/triage/c", text: "http://viewer/api/triage/c/runs/run9/text" }
}

const report: RunReport.RunReport = {
  schema_version: RunReport.SCHEMA_VERSION,
  selector: "@run:9",
  source: "summaries",
  headline: "1 call: 0 passed, 1 failed, 0 skipped",
  runs: [{
    id: 9,
    capture: "c",
    lane: "fake",
    setKind: "full",
    status: "failed",
    env: "main-checkout",
    branch: "main",
    code: [{ name: "app", sha: "a".repeat(40) }],
    sweepId: 3,
    jobId: null,
    startedAt: "2026-10-01T10:00:00.000Z",
    finishedAt: null,
    cases: 1,
    failed: 1,
    citedBy: ["tracker/12"]
  }],
  total: 1,
  side: {
    selector: "@run:9",
    runs: 1,
    cells: 1,
    lanes: ["fake"],
    envs: ["main-checkout"],
    branches: ["main"],
    components: [{ name: "app", shas: ["a".repeat(40)] }],
    laneHashes: [null],
    sweeps: []
  },
  counts: { calls: 1, passed: 0, failed: 1, skipped: 0 },
  calls: [{ capture: "c", caseId: "c-case1", cell }]
}

describe("the run report format", () => {
  it("is published beside the compare report", () => {
    expect(Object.keys(PUBLISHED)).toContain("run-report.schema.json")
    const text = PUBLISHED["run-report.schema.json"]!()
    expect(fs.readFileSync(path.join(import.meta.dirname, "..", "schemas", "run-report.schema.json"), "utf8")).toBe(text)
  })

  it("decodes a document of the format, the summaries' source included", () => {
    expect(RunReport.decode(JSON.parse(JSON.stringify(report)))).toEqual(report)
  })

  it("refuses another version, an unknown source and a fractional count", () => {
    expect(() => RunReport.decode({ ...report, schema_version: "sip.run-report/2" })).toThrow()
    expect(() => RunReport.decode({ ...report, source: "bundles" })).toThrow()
    expect(() => RunReport.decode({ ...report, counts: { ...report.counts, failed: 0.5 } })).toThrow()
  })

  it("states its version in the published schema", () => {
    const schema = RunReport.jsonSchema() as { properties: { schema_version: { enum: ReadonlyArray<string> } } }
    expect(schema.properties.schema_version.enum).toEqual([RunReport.SCHEMA_VERSION])
  })
})
