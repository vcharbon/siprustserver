import { describe, expect, it } from "vitest"
import type { CampaignCase, CampaignRun, LaunchTag } from "../src/api.ts"
import { asked, carriedBy, exclusionsOf, offered, taken } from "../src/campaign-tags.ts"

const tag = (name: string, excludedOn: readonly string[]): LaunchTag => ({
  tag: name,
  label: `label of ${name}`,
  warning: null,
  excludedOn,
  allowed: false
})

const carrying = (...tags: readonly string[]): CampaignCase => ({
  caseId: tags.join("+"),
  ref: tags.join("+"),
  subcase: null,
  family: "f",
  title: null,
  description: null,
  groups: [],
  tags,
  links: [],
  source: null
})

const TAGS = [tag("slow", ["real"]), tag("harsh", ["real", "lab"]), tag("free", [])]

const run = (id: number, scope: string | null, excluded: readonly string[]): CampaignRun => ({
  id,
  env: null,
  startedAt: "2026-11-06T00:00:00.000Z",
  finishedAt: null,
  labels: [],
  scope,
  selection: null,
  checked: {},
  lanes: [{
    lane: "real",
    runId: id,
    session: `run${id}`,
    status: "done",
    detail: null,
    skipped: excluded.length,
    excluded: excluded.map((caseId) => ({ caseId, reason: `excluded: ${caseId}` })),
    results: []
  }]
})

describe("why a case was left out", () => {
  it("reads the newest whole run, whatever a case with no verdict has to show", () => {
    expect(exclusionsOf([run(2, null, ["c"]), run(1, null, [])], "c")).toEqual([{ lane: "real", reason: "excluded: c" }])
  })

  it("is nothing once the newest whole run took the case, and a partial run changes nothing", () => {
    expect(exclusionsOf([run(1, null, ["c"]), run(2, null, [])], "c")).toEqual([])
    expect(exclusionsOf([run(1, null, ["c"]), run(2, "c", [])], "c")).toEqual([{ lane: "real", reason: "excluded: c" }])
  })

  it("is nothing where no whole run is read", () => {
    expect(exclusionsOf([run(1, "c", ["c"])], "c")).toEqual([])
    expect(exclusionsOf([], "c")).toEqual([])
  })
})

describe("the tags a campaign start offers", () => {
  it("are those a picked lane excludes, counted over the cases carrying them", () => {
    const offers = offered(TAGS, ["real"], [carrying("slow"), carrying("slow", "harsh"), carrying("free")])
    expect(offers.map((offer) => [offer.tag, offer.cases])).toEqual([["slow", 2], ["harsh", 1]])
    expect(offered(TAGS, ["lab"], []).map((offer) => offer.tag)).toEqual(["harsh"])
    expect(offered(TAGS, ["other"], [])).toEqual([])
  })

  it("ask only for what the picked lanes still exclude", () => {
    const offers = offered(TAGS, ["lab"], [])
    expect(asked(["slow", "harsh"], offers)).toEqual(["harsh"])
    expect(asked([], offers)).toEqual([])
    const fixed = offered([{ ...tag("own", ["lab"]), allowed: true }, tag("harsh", ["lab"])], ["lab"], [])
    expect(asked(["own", "harsh"], fixed)).toEqual(["harsh"])
    expect(taken([], fixed).map((offer) => offer.tag)).toEqual(["own"])
    expect(taken(["harsh"], fixed).map((offer) => offer.tag)).toEqual(["own", "harsh"])
  })

  it("name a case's tags as the server declares them, an undeclared one left out", () => {
    expect(carriedBy(TAGS, ["harsh", "gone", "slow"]).map((one) => one.tag)).toEqual(["harsh", "slow"])
  })
})
