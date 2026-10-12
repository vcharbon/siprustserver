/**
 * The study page's head, in its two modes: the capture whole — its name and
 * the counts of its calls — and ONE call of it, named by its case id over a
 * one-line pointer up to the capture study, which is what a call opened from
 * the queue reads first.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { TriageStudy } from "../src/api.ts"
import { StudyHead } from "../src/components/StudyHead.tsx"

const CAPTURE = "capture_demo.pcap.gz"
const CASE = "capture_demo-case1"

const study: TriageStudy = {
  capture: CAPTURE,
  disposition: "untriaged",
  record: null,
  standing: {
    lane: "fake",
    runId: 1,
    prepId: 1,
    bundlePath: "/nowhere/run1",
    cases: 2,
    failed: 1,
    failedSteps: ["s3"],
    caseResults: [
      { caseId: CASE, passed: false, failedStep: "s3", skipped: null },
      { caseId: "capture_demo-case2", passed: true, failedStep: null, skipped: null }
    ],
    rfcGating: 0,
    finishedAt: "2026-09-15T10:00:00.000Z",
    lastOkRunId: null,
    lastOkBundlePath: null,
    caseRuns: []
  },
  cut: { prepId: 1, cutAt: "2026-09-15T00:00:00.000Z", broke: null, cases: 2 },
  auto: null,
  exclusions: {
    path: "/nowhere/excluded.json",
    document: [{ caseId: "capture_demo-cut3", reason: "source-truncated-at-edge", callIds: ["c3"] }]
  },
  cases: [],
  sessions: [],
  repoRoot: "/nowhere"
}

describe("the study head", () => {
  it("names the capture and counts its calls for the capture study", () => {
    const markup = renderToStaticMarkup(<StudyHead study={study} lane="fake" />)
    expect(markup).toContain(`<h1>`)
    expect(markup).toContain(CAPTURE)
    expect(markup).toContain("3 calls")
    expect(markup).toContain("1 refused")
    expect(markup).toContain("1 red")
    expect(markup).not.toContain("one call of")
  })

  it("names the one call and links up to the capture study for a one-call study", () => {
    const markup = renderToStaticMarkup(
      <StudyHead study={{ ...study, case: CASE }} lane="fake" cell={`run1/${CASE}__pivot-replay__fake`} />
    )
    // The title is the call; the capture is one line under it, as a link.
    expect(markup).toMatch(new RegExp(`<h1>.*${CASE}.*</h1>`))
    expect(markup).toContain("one call of")
    expect(markup).toContain(
      `href="/triage/${encodeURIComponent(CAPTURE)}?lane=fake&amp;cell=run1%2F${CASE}__pivot-replay__fake"`
    )
    // The capture's counts are the capture study's, not this page's.
    expect(markup).not.toContain("3 calls")
  })

  it("links a refused call up to the capture study with no cell", () => {
    const markup = renderToStaticMarkup(<StudyHead study={{ ...study, case: "capture_demo-cut3" }} lane="fake" />)
    expect(markup).toContain(`href="/triage/${encodeURIComponent(CAPTURE)}?lane=fake"`)
  })
})
