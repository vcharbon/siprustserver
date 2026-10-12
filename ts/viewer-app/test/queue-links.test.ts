/**
 * The two links of a queue row: the `capture` cell opens the capture study,
 * the `call` cell opens the study of that one call — for a case row and for a
 * refused row alike, on the lane the queue was read on, at the step the call
 * died at where the run named one.
 */
import { describe, expect, it } from "vitest"
import type { TriageCallRow } from "../src/api.ts"
import { callStudyOf, captureStudyOf, deltasOf } from "../src/queue-links.ts"

const row = (over: Partial<TriageCallRow>): TriageCallRow => ({
  capture: "capture_a.pcap.gz",
  caseId: "capture_a-case1",
  kind: "case",
  callKey: "a@key",
  runId: 7,
  setKind: "full",
  prepId: null,
  bundlePath: "/nowhere/run7",
  documentHash: "h1",
  passed: false,
  skipped: null,
  failedStep: null,
  rfcGating: 0,
  reasons: [],
  features: [],
  mainError: null,
  finishedAt: null,
  family: null,
  dialled: null,
  outcome: null,
  legs: null,
  attempts: null,
  bundle: true,
  ...over
})

const died = row({
  failedStep: "s14",
  mainError: { kind: "died", rule: null, rfc: null, step: "s14", expected: "200 to INVITE", detail: "" }
})

const refused = row({
  caseId: "capture_a-cut2",
  kind: "refused",
  runId: null,
  setKind: null,
  prepId: 3,
  bundlePath: null,
  documentHash: null,
  reasons: ["source-truncated-at-edge"],
  mainError: { kind: "refused", rule: "source-truncated-at-edge", rfc: null, step: null, expected: null, detail: "" }
})

describe("a queue row's links", () => {
  it("opens the capture study from the capture cell, whatever the row", () => {
    expect(captureStudyOf(died, "fake")).toBe("/triage/capture_a.pcap.gz?lane=fake")
    expect(captureStudyOf(refused, "fake")).toBe("/triage/capture_a.pcap.gz?lane=fake")
  })

  it("opens the study of the one call from the call cell, at the step it died at", () => {
    expect(callStudyOf(died, "fake"))
      .toBe("/triage/capture_a.pcap.gz?lane=fake&case=capture_a-case1&step=s14")
    // The main error's step outranks the verdict's; a passed call names none.
    expect(callStudyOf(row({ failedStep: "s3" }), "fake"))
      .toBe("/triage/capture_a.pcap.gz?lane=fake&case=capture_a-case1&step=s3")
    expect(callStudyOf(row({ passed: true }), "")).toBe("/triage/capture_a.pcap.gz?case=capture_a-case1")
  })

  it("opens the study of a refused call too, which shows its refusal", () => {
    expect(callStudyOf(refused, "fake"))
      .toBe("/triage/capture_a.pcap.gz?lane=fake&case=capture_a-cut2")
  })
})

describe("the queue's link to its delta sheet", () => {
  it("carries the slice the queue reads — lane, scope, needle, includes and excludes — and no row cap", () => {
    const url = new URL(
      deltasOf({
        lane: "lab",
        scope: "all",
        q: "A101",
        filter: {
          include: { rule: "", expected: "", refusal: "", outcome: "failed", feature: "prack" },
          exclude: { rule: ["bad-body"], expected: [], refusal: [], outcome: [], feature: ["mrf", "reroute"] }
        }
      }),
      "http://viewer"
    )
    expect(url.pathname).toBe("/deltas")
    expect([...url.searchParams.entries()]).toEqual([
      ["lane", "lab"],
      ["scope", "all"],
      ["q", "A101"],
      ["outcome", "failed"],
      ["feature", "prack"],
      ["not-rule", "bad-body"],
      ["not-feature", "mrf"],
      ["not-feature", "reroute"]
    ])
  })
})
