/**
 * The compare report format: the published JSON Schema is the one the schema
 * generates, a document of the format decodes, and a document of another
 * version or with a class outside the format does not.
 */
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as CompareReport from "../src/compare-report.js"
import { PUBLISHED } from "../src/schemas.js"

const cell: CompareReport.Cell = {
  run: 2,
  lane: "fake",
  env: "main-checkout",
  branch: "main",
  code: [{ name: "app", sha: "a".repeat(40) }, { name: "core", sha: null }],
  verdict: "ok",
  passed: true,
  repeats: { runs: 1, passed: 1 },
  skipped: null,
  failedStep: null,
  mainError: null,
  bless: "clean",
  accepted: 3,
  knownBugs: [],
  ladder: null,
  links: { study: "http://viewer/triage/c", text: "http://viewer/api/triage/c/runs/run2/text" }
}

const zero = { sameDocument: 0, documentChanged: 0 }

const report: CompareReport.CompareReport = {
  schema_version: CompareReport.SCHEMA_VERSION,
  headline: "same code",
  code: "same",
  compared: 1,
  unexplained: 0,
  before: {
    selector: "@run:1",
    runs: 1,
    cells: 1,
    lanes: ["fake"],
    envs: ["main-checkout"],
    branches: ["main"],
    components: [{ name: "app", shas: ["a".repeat(40)] }],
    laneHashes: [null],
    sweeps: [{ id: 7, universe: "named", selection: null, captures: 1 }]
  },
  after: {
    selector: "@run:2",
    runs: 1,
    cells: 1,
    lanes: ["fake"],
    envs: ["dev-1"],
    branches: ["fix/1"],
    components: [{ name: "app", shas: [null] }],
    laneHashes: ["l1"],
    sweeps: []
  },
  counts: Object.fromEntries(CompareReport.MOVE_CLASSES.map((one) => [one, one === "red→green" ? { ...zero, sameDocument: 1 } : zero])) as CompareReport.CompareReport["counts"],
  onlyBefore: { captures: 0, count: 0, calls: [] },
  onlyAfter: { captures: 1, count: 2, calls: ["c/c-case2"] },
  calls: [{
    alias: "c-case1",
    capture: "c",
    caseId: "c-case1",
    class: "red→green",
    document: "same",
    code: "same",
    lane: "unknown",
    before: {
      ...cell,
      run: 1,
      passed: false,
      verdict: "failed",
      failedStep: "s6",
      mainError: { kind: "died", rule: null, rfc: null, step: "s6", expected: "200 to INVITE", detail: "" }
    },
    after: cell,
    ref: ["A  B", "|--INVITE-->|"],
    delta: {
      firstDifference: { line: 4, before: "uas1 -> SUT | 200 OK | INVITE · CSeq +0", after: null },
      differingLines: 1
    }
  }]
}

const decode = Schema.decodeUnknownSync(CompareReport.CompareReport)

describe("the compare report format", () => {
  it("publishes the JSON Schema it generates (`pnpm schemas` rewrites it)", () => {
    for (const [file, text] of Object.entries(PUBLISHED)) {
      expect(fs.readFileSync(path.join(import.meta.dirname, "..", "schemas", file), "utf8")).toBe(text())
    }
  })

  it("decodes a document of the format", () => {
    expect(decode(JSON.parse(JSON.stringify(report)))).toEqual(report)
  })

  it("refuses a document of another version, or naming a class outside the format", () => {
    expect(() => decode({ ...report, schema_version: "sip.compare/2" })).toThrow()
    expect(() => decode({ ...report, calls: [{ ...report.calls[0], class: "moved" }] })).toThrow()
    expect(() => decode({ ...report, compared: 1.5 })).toThrow()
  })

  it("carries each call's ladder delta, null where either ladder was not read", () => {
    const call = report.calls[0]!
    expect(decode({ ...report, calls: [{ ...call, delta: null }] }).calls[0]?.delta).toBeNull()
    expect(() => decode({ ...report, calls: [{ ...call, delta: { firstDifference: null } }] })).toThrow()
    expect(() => decode({ ...report, calls: [{ ...call, delta: { firstDifference: null, differingLines: -1 } }] }))
      .toThrow()
  })

  it("states its version in the published schema", () => {
    const schema = CompareReport.jsonSchema() as { properties: { schema_version: { enum: ReadonlyArray<string> } } }
    expect(schema.properties.schema_version.enum).toEqual([CompareReport.SCHEMA_VERSION])
  })
})
