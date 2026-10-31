/** A case's checks as its details list them: each marked per lane of the newest run that judged it, unlisted failures after. */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignCheck, CampaignResult } from "../src/api.ts"
import { checkList } from "../src/campaign-checks.ts"

const failed = (anchor: string, field: string): CampaignCheck => ({
  anchor,
  field,
  expected: "e",
  seen: "s",
  said: null,
  side: "sut"
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
  results
})

const view = (lanes: ReturnType<typeof lane>[]): Campaign => ({
  capture: "c",
  env: null,
  envs: [],
  saidLabel: "said",
  sides: [],
  lanes: lanes.map((l) => l.lane),
  cases: [{
    caseId: "c",
    ref: "c",
    subcase: null,
    family: "f",
    title: null,
    description: null,
    groups: [],
    links: [],
    source: null,
    checked: [
      { anchor: "B1 INVITE", field: "ruri.uri", op: "eq", expected: "sip:1@h" },
      { anchor: "A final", field: "status", op: "eq", expected: "200" },
      { anchor: "rfc", field: "*", op: "audit", expected: "no gating finding" }
    ]
  }],
  runs: [{ id: 2, env: null, startedAt: "", finishedAt: null, labels: [], lanes }]
})

describe("a case's check list", () => {
  it("marks every listed check passed on a lane that judged the case clean", () => {
    const list = checkList(view([lane("x", [result("pass")])]), "c", ["x"])
    expect(list.run).toBe(2)
    expect(list.rows.map((r) => [r.anchor, r.field, r.marks["x"]])).toEqual([
      ["B1 INVITE", "ruri.uri", "pass"],
      ["A final", "status", "pass"],
      ["rfc", "*", "pass"]
    ])
  })

  it("marks a check failed on its lane only, a `*` check by any field at its anchor, none where the lane judged nothing", () => {
    const list = checkList(
      view([
        lane("x", [result("failure", [failed("B1 INVITE", "ruri.uri"), failed("rfc", "some-rule")])]),
        lane("y", [result("pass")]),
        lane("z", [result("error")])
      ]),
      "c",
      ["x", "y", "z"]
    )
    expect(list.rows.map((r) => [r.field, r.marks])).toEqual([
      ["ruri.uri", { x: "failure", y: "pass", z: "none" }],
      ["status", { x: "pass", y: "pass", z: "none" }],
      ["*", { x: "failure", y: "pass", z: "none" }]
    ])
  })

  it("lists a failed check no listed one matches after the list, once, on every lane it failed on", () => {
    const flow = failed("B2 INVITE", "unmatched-datagram")
    const list = checkList(view([lane("x", [result("failure", [flow])]), lane("y", [result("failure", [flow])])]), "c", ["x", "y"])
    expect(list.rows.slice(3).map((r) => [r.anchor, r.field, r.op, r.marks])).toEqual([
      ["B2 INVITE", "unmatched-datagram", "", { x: "failure", y: "failure" }]
    ])
  })
})
