/**
 * A job's red cell links where it is read: a capture replay's call to its
 * triage study, a campaign's case to its call details on the campaign page,
 * never to a triage study of the campaign's name, which holds no such case.
 */
import { describe, expect, it } from "vitest"
import type { JobRedCell } from "../src/api.ts"
import { readState } from "../src/campaign-state.ts"
import { cellHref, holderHref } from "../src/job-cell-links.ts"

const RED: JobRedCell = {
  capture: "cap-1.anon",
  lane: "lane-x",
  caseId: "cap-1.anon-case1",
  runId: 3,
  error: null,
  bodyFailing: 0,
  campaign: null
}

const query = (link: string): URLSearchParams => new URL(link, "http://h").searchParams
const pathOf = (link: string): string => new URL(link, "http://h").pathname

describe("a job's red cell", () => {
  it("of a capture replay opens the capture's study and the call's on its lane", () => {
    expect(pathOf(holderHref(RED))).toBe("/triage/cap-1.anon")
    expect(query(holderHref(RED)).get("lane")).toBe("lane-x")
    expect(pathOf(cellHref(RED))).toBe("/triage/cap-1.anon")
    expect(query(cellHref(RED)).get("case")).toBe("cap-1.anon-case1")
  })

  it("of a campaign opens its run's page and the case's call details on its run and lane", () => {
    const cell: JobRedCell = { ...RED, capture: "camp-a", caseId: "fam/one [s]", runId: 12, campaign: { run: 4, env: "env-a" } }
    expect(pathOf(holderHref(cell))).toBe("/campaign/camp-a")
    expect(readState(query(holderHref(cell)))).toMatchObject({ env: "env-a", run: 4, caseId: undefined, delta: undefined })
    expect(pathOf(cellHref(cell))).toBe("/campaign/camp-a")
    expect(readState(query(cellHref(cell)))).toMatchObject({
      env: "env-a",
      run: 4,
      caseId: "fam/one [s]",
      delta: { run: 4, lane: "lane-x" }
    })
  })
})
