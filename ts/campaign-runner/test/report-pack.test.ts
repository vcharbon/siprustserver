/**
 * A run's report under the pack's report facts: the failure type its sides
 * make, a testcase's class off its ref, how a failed check names what the
 * backend said, the log's name, the run properties it adds, what a quick
 * run's cells state, and how an absent universe is said.
 */
import type { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { junitXml } from "../src/junit.js"
import { lines, type ReportPack } from "../src/report.js"
import type * as Results from "@sip/store/campaign-results"
import { notPorted } from "../src/universe.js"

interface Run extends Results.CampaignRun {
  readonly target: string
}

const PACK: ReportPack<Run> = {
  failureType: (sides) => (sides.includes("left") ? "left-side" : "other-side"),
  classOf: (ref) => `suite.${ref.split("/")[0]}`,
  runClass: "suite.run",
  said: "backend",
  log: "backend log",
  properties: (run) => [["target", run.target], ["absent", null]],
  universe: {
    where: Effect.succeed("/u"),
    refs: () => Effect.succeed(undefined),
    absent: (where) => `no universe in ${where}`,
    looked: "its index"
  },
  quickNotes: ["deep pass: not run (--quick)"]
}

const failed = {
  name: "call",
  source: "interpreter",
  answer: "fail",
  detail: { anchor: "B1 INVITE", field: "ruri", expected: "sip:a@h", seen: "sip:b@h", said: "ruri = sip:a@h", side: "left" }
} as const satisfies Verification.Verification

const result = (caseId: string, rest: Partial<Results.Result>): Results.Result => ({
  caseId,
  cell: "c1",
  log: { missing: "store down" },
  passed: false,
  skipped: null,
  verdict: "fail",
  errorKind: null,
  errorRule: null,
  errorDetail: null,
  checks: [],
  verifications: [failed],
  ...rest
})

const RUN: Run = {
  id: 3,
  capture: "camp",
  env: "e1",
  branch: null,
  seed: null,
  target: "backend:v2",
  dir: "/runs/3",
  scope: null,
  selection: null,
  startedAt: "2026-11-01T00:00:00.000Z",
  finishedAt: null,
  lanes: [{
    runId: 9,
    lane: "sim",
    status: "error",
    detail: "the lane stood no SUT",
    bundlePath: "/s/run9",
    components: [],
    results: [result("family/one [a]", {})]
  }]
}

describe("a run's console report under its pack", () => {
  it("types a failure, names its log and a quick run's notes, and says the universe absent as the pack words it", () => {
    expect(lines(PACK, RUN, true, "/u")).toEqual([
      "FAIL  sim family/one [a] [left-side]  /s/run9/c1",
      "        [left] call: expected sip:a@h, seen sip:b@h",
      "        backend log missing: store down",
      "        deep pass: not run (--quick)",
      "ERROR sim run: the lane stood no SUT",
      "sim: 0 passed, 1 failed, 0 errored, not ported unknown: no universe in /u"
    ])
  })

  it("counts the refs of a universe no case file ports, sorted", () => {
    expect(notPorted(["b", "a", "c"], ["c"])).toEqual(["a", "b"])
  })
})

describe("a run's JUnit report under its pack", () => {
  const xml = junitXml(PACK, RUN, { verb: "rig run", site: "http://site" })

  it("classes a case off its ref and a lane's unreached run by the pack's run class", () => {
    expect(xml).toContain(`<testcase classname="suite.family" name="family/one [a]">`)
    expect(xml).toContain(`<testcase classname="suite.run" name="sim run">`)
  })

  it("states what the backend said under the pack's label and the log missing under its name", () => {
    expect(xml).toContain("      backend   ruri = sip:a@h")
    expect(xml).toContain("backend log missing: store down")
    expect(xml).toContain(`<failure type="left-side"`)
  })

  it("names the run by the pack's verb, and adds the pack's run properties after the seed, leaving a null one out", () => {
    expect(xml).toContain(`<property name="rig-run" value="${RUN.id}"/>`)
    expect(xml).toContain(`<property name="target" value="backend:v2"/>`)
    expect(xml).not.toContain(`name="absent"`)
  })
})
