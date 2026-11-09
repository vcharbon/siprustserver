/**
 * A case's checks as its details list them: the list of the newest run that
 * judged the case, each check marked per lane by its own failures only, the
 * failures no listed check owns after.
 */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignCheck, CampaignChecked, CampaignResult } from "../src/api.ts"
import { checkList } from "../src/campaign-checks.ts"

const failed = (anchor: string, field: string, step: string | null, expected = "e"): CampaignCheck => ({
  anchor,
  field,
  expected,
  seen: "s",
  said: null,
  side: "sut",
  step
})

const result = (outcome: CampaignResult["outcome"], checks: readonly CampaignCheck[] = []): CampaignResult => ({
  caseId: "c",
  outcome,
  cell: null,
  log: null,
  detail: null,
  checks
})

const lane = (name: string, results: readonly CampaignResult[]) => ({
  lane: name,
  runId: 1,
  session: "s",
  status: "done",
  detail: null,
  skipped: 0,
  excluded: [],
  results
})

const at = (step: string | null, anchor: string, field: string, op: string, expected: string | null): CampaignChecked => ({
  step,
  anchor,
  field,
  op,
  expected
})

const LISTED: readonly CampaignChecked[] = [
  at("s3", "B1 INVITE", "*", "arrives", "INVITE"),
  at("s3", "B1 INVITE", "ruri.uri", "eq", "sip:1@{B1 host}"),
  at("s5", "B1 NOTIFY", "*", "arrives", "NOTIFY"),
  at("s5", "B1 NOTIFY", "body", "regex", "100"),
  at("s7", "B1 NOTIFY", "*", "arrives", "NOTIFY"),
  at("s7", "B1 NOTIFY", "body", "regex", "486"),
  at("s9", "A final", "status", "eq", "200"),
  at(null, "rfc", "*", "audit", "no gating finding")
]

type Run = { readonly id: number; readonly lanes: ReturnType<typeof lane>[]; readonly checked?: readonly CampaignChecked[] }

const view = (...runs: Run[]): Campaign => ({
  capture: "c",
  title: null,
  env: null,
  envs: [],
  saidLabel: "said",
  sides: [],
  lanes: [...new Set(runs.flatMap((r) => r.lanes.map((l) => l.lane)))],
  cases: [{ caseId: "c", ref: "c", subcase: null, family: "f", title: null, description: null, groups: [], tags: [], links: [], source: null }],
  launch: null,
  own: null,
  runs: runs.map((r) => ({
    id: r.id,
    env: null,
    startedAt: "",
    finishedAt: null,
    labels: [],
    lanes: r.lanes,
    scope: null,
    selection: null,
    checked: { c: r.checked ?? LISTED }
  }))
})

const failing = (rows: ReturnType<typeof checkList>["rows"], lane: string) =>
  rows.flatMap((r, i) => (r.marks[lane] === "failure" ? [`${i} ${r.anchor} ${r.field}`] : []))

describe("a case's check list", () => {
  it("marks every listed check passed on a lane that judged the case clean", () => {
    const list = checkList(view({ id: 2, lanes: [lane("x", [result("pass")])] }), "c", ["x"])
    expect(list.run).toBe(2)
    expect(list.rows.length).toBe(LISTED.length)
    expect(list.rows.every((r) => r.marks["x"] === "pass")).toBe(true)
  })

  it("marks only the failed check's own row: one of several checks of one field, by its step and its expected value", () => {
    const list = checkList(
      view({ id: 2, lanes: [lane("x", [result("failure", [failed("B1 NOTIFY", "body", "s7", "486")])])] }),
      "c",
      ["x"]
    )
    expect(failing(list.rows, "x")).toEqual(["5 B1 NOTIFY body"])
  })

  it("puts a flow failure on its step's arrival, a gate's on the gate's `*`, nothing on the other lanes", () => {
    const list = checkList(
      view({
        id: 2,
        lanes: [
          lane("x", [result("failure", [failed("B1 INVITE", "unmatched-datagram", "s3"), failed("rfc", "some-rule", null)])]),
          lane("y", [result("pass")]),
          lane("z", [result("error")])
        ]
      }),
      "c",
      ["x", "y", "z"]
    )
    expect(failing(list.rows, "x")).toEqual(["0 B1 INVITE *", "7 rfc *"])
    expect(list.rows.every((r) => r.marks["y"] === "pass" && r.marks["z"] === "none")).toBe(true)
  })

  it("lists a failure no listed check owns, and a lane that failed naming none, after the list", () => {
    const list = checkList(
      view({
        id: 2,
        lanes: [lane("x", [result("failure", [failed("call", "timing", null)])]), lane("y", [result("failure")])]
      }),
      "c",
      ["x", "y"]
    )
    expect(list.rows.slice(LISTED.length).map((r) => [r.anchor, r.field, r.marks])).toEqual([
      ["call", "timing", { x: "failure", y: "pass" }],
      ["call", "verdict", { x: "pass", y: "failure" }]
    ])
  })

  it("reads the list in the run whose marks it shows, not a newer run that judged nothing", () => {
    const list = checkList(
      view(
        { id: 3, lanes: [lane("x", [])], checked: [...LISTED, at("s11", "A BYE", "*", "arrives", "BYE")] },
        { id: 2, lanes: [lane("x", [result("pass")])] }
      ),
      "c",
      ["x"]
    )
    expect([list.run, list.rows.length]).toEqual([2, LISTED.length])
  })
})
