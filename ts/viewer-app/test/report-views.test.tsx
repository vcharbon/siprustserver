/**
 * The compare and run report views draw the documents as served: the
 * headline, both sides, the class counts and each call with both cells and
 * the three ladders side by side; a report of a summarised run says first
 * that its bundle and row are gone. Links lead into this viewer, wherever the
 * producer named it.
 */
import type { CompareReport, RunReport } from "@sip/contracts"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { CompareReportView, localPath } from "../src/components/CompareReportView.tsx"
import { RunReportView } from "../src/components/RunReportView.tsx"

const cell = (run: number, passed: boolean, ladder: ReadonlyArray<string> | null): CompareReport.Cell => ({
  run,
  lane: "fake",
  env: run === 1 ? "main-checkout" : "dev-7-x",
  branch: run === 1 ? "main" : "feat/7-x",
  code: [{ name: "app", sha: "a".repeat(40) }],
  verdict: passed ? "ok" : "failed",
  passed,
  repeats: { runs: 1, passed: passed ? 1 : 0 },
  skipped: null,
  failedStep: passed ? null : "s6",
  mainError: passed
    ? null
    : { kind: "died", rule: "expect-timed-out", rfc: null, step: "s6", expected: "200 to INVITE", detail: "" },
  bless: "clean",
  accepted: 2,
  knownBugs: [],
  ladder,
  links: {
    study: `http://localhost:5175/triage/c.pcap?lane=fake&case=c-case1&cell=run${run}%2Fc-case1__pivot-replay__fake`,
    text: `http://localhost:5175/api/triage/c.pcap/runs/run${run}/text?lane=fake&case=c-case1`
  }
})

const side = (selector: string): CompareReport.Side => ({
  selector,
  runs: 1,
  cells: 1,
  lanes: ["fake"],
  envs: ["main-checkout"],
  branches: ["main"],
  components: [{ name: "app", shas: ["a".repeat(40)] }],
  laneHashes: ["l1"],
  sweeps: [{ id: 11, universe: "named", selection: "c.pcap", captures: 1 }]
})

const zero = { sameDocument: 0, documentChanged: 0 }

/** A two-event replay ladder whose answer is `status`. */
const ladderOf = (status: string): ReadonlyArray<string> => [
  "run c-case3",
  "",
  "ladder (2 events; m<idx> +t from -> to | message | detail | dialog | step):",
  "  m0 +0.000000 uac1 -> SUT | INVITE | CSeq 1 | fromA1 | s1",
  `  m1 +0.010000 SUT -> uac1 | ${status} | INVITE · CSeq 1 | fromA1 · toA1 | s2`
]

const compare: CompareReport.CompareReport = {
  schema_version: "sip.compare/1",
  headline: "the two sides ran DIFFERENT code",
  code: "different",
  compared: 1,
  unexplained: 0,
  before: side("@develop/@lane:fake"),
  after: side("@env:dev-7-x/@lane:fake"),
  counts: {
    "green→red": zero,
    "red→green": { sameDocument: 1, documentChanged: 0 },
    "red→red-moved": zero,
    "ran→skipped": zero,
    "skipped→ran": zero,
    "clean→known-bug": zero,
    "known-bug→clean": zero,
    unchanged: zero,
    unstable: { sameDocument: 1, documentChanged: 0 }
  },
  onlyBefore: { captures: 0, count: 0, calls: [] },
  onlyAfter: { captures: 2, count: 1, calls: ["c.pcap/c-case2"] },
  calls: [{
    alias: "c-case1",
    capture: "c.pcap",
    caseId: "c-case1",
    class: "red→green",
    document: "same",
    code: "different",
    lane: "same",
    before: cell(1, false, ["run1 c-case1", "A -- INVITE --> B"]),
    after: cell(2, true, null),
    ref: ["c.pcap c-case1", "A -- INVITE --> SUT"],
    delta: null
  }, {
    alias: "c-case3",
    capture: "c.pcap",
    caseId: "c-case3",
    class: "unstable",
    document: "same",
    code: "different",
    lane: "same",
    before: cell(1, true, ladderOf("200 OK")),
    after: { ...cell(2, false, ladderOf("486 Busy Here")), repeats: { runs: 3, passed: 2 } },
    ref: null,
    delta: {
      firstDifference: {
        line: 1,
        before: "SUT -> uac1 | 200 OK | INVITE · CSeq +0 | fromA1 · toA1 | s2",
        after: "SUT -> uac1 | 486 Busy Here | INVITE · CSeq +0 | fromA1 · toA1 | s2"
      },
      differingLines: 1
    }
  }]
}

describe("the compare view", () => {
  const markup = renderToStaticMarkup(<CompareReportView doc={compare} />)

  it("draws the headline, both sides and only the classes that hold calls", () => {
    expect(markup).toContain("the two sides ran DIFFERENT code")
    expect(markup).toContain("@env:dev-7-x/@lane:fake")
    expect(markup).toContain("11 (c.pcap)")
    expect(markup).toContain("red→green")
    expect(markup).not.toContain("green→red")
    expect(markup).toContain("2 capture(s) the other side never ran; 1 call(s)")
  })

  it("draws each call with both cells and the three ladders side by side", () => {
    expect(markup).toContain("failed at s6")
    expect(markup).toContain("died expect-timed-out at s6 waiting 200 to INVITE")
    expect(markup).toContain("A -- INVITE --&gt; B")
    expect(markup).toContain("A -- INVITE --&gt; SUT")
    expect(markup.match(/class="compare-ladder"/g)).toHaveLength(3 * compare.calls.length)
    // The after cell's bundle is gone: its ladder says it was not read.
    expect(markup).toContain("not read: the bundle is gone")
  })

  it("names an unstable call and the repeats its side ran it in", () => {
    expect(markup).toContain("1 unstable call(s): repeats disagree")
    expect(markup).toContain(`<span class="tag">unstable</span> <code>c-case3</code>`)
    expect(markup).toContain("repeats 2/3 passed")
    // A cell run once states no repeats.
    expect(markup.match(/repeats \d+\/\d+ passed/g)).toHaveLength(1)
  })

  it("marks the first line where the two ladders differ, on both sides, and says how many lines differ", () => {
    expect(markup).toContain(
      `<mark class="ladder-first-difference">  m1 +0.010000 SUT -&gt; uac1 | 200 OK | INVITE · CSeq 1 | fromA1 · toA1 | s2</mark>`
    )
    expect(markup).toContain(
      `<mark class="ladder-first-difference">  m1 +0.010000 SUT -&gt; uac1 | 486 Busy Here | INVITE · CSeq 1 | fromA1 · toA1 | s2</mark>`
    )
    expect(markup.match(/<mark class="ladder-first-difference">/g)).toHaveLength(2)
    expect(markup).toContain("ladder: 1 line(s) differ, first at m1")
    // A call whose ladders were not both read states no delta.
    expect(markup.match(/ladder: \d+ line\(s\) differ/g)).toHaveLength(1)
  })

  it("links each cell into this viewer, and each side to its report", () => {
    expect(markup).toContain(`href="/triage/c.pcap?lane=fake&amp;case=c-case1&amp;cell=run2%2Fc-case1__pivot-replay__fake"`)
    expect(markup).toContain(`href="/api/triage/c.pcap/runs/run1/text?lane=fake&amp;case=c-case1"`)
    expect(markup).toContain(`href="/report?run=%40develop%2F%40lane%3Afake"`)
    expect(localPath("http://elsewhere:9/x?y=1")).toBe("/x?y=1")
  })
})

const summarised: RunReport.RunReport = {
  schema_version: "sip.run-report/1",
  selector: "run1051725",
  source: "summaries",
  headline: "2 calls: 1 passed, 1 failed, 0 skipped",
  runs: [{
    id: 1051725,
    capture: "c.pcap",
    lane: "fake",
    setKind: "full",
    status: "done",
    env: "main-checkout",
    branch: "main",
    code: [],
    sweepId: 7,
    jobId: null,
    startedAt: "2026-09-20T08:00:00.000Z",
    finishedAt: null,
    cases: 2,
    failed: 1,
    citedBy: ["tracker/12"]
  }],
  total: 1,
  side: side("run1051725"),
  counts: { calls: 2, passed: 1, failed: 1, skipped: 0 },
  calls: [
    { capture: "c.pcap", caseId: "c-case1", cell: { ...cell(1051725, true, null), bless: "unmeasured" } },
    { capture: "c.pcap", caseId: "c-case2", cell: { ...cell(1051725, false, null), bless: "unmeasured" } }
  ]
}

describe("the report view", () => {
  const markup = renderToStaticMarkup(<RunReportView doc={summarised} />)

  it("says first that a summarised run's bundle and row are gone, and who cites it", () => {
    expect(markup.indexOf("summary kept by a citation")).toBeLessThan(markup.indexOf("2 calls: 1 passed"))
    expect(markup).toContain("run 1051725&#x27;s bundle and row")
    expect(markup).toContain("tracker/12")
  })

  it("lists the failed calls first, each with its main error and its evidence links", () => {
    expect(markup.indexOf("c-case2")).toBeLessThan(markup.indexOf("c-case1"))
    expect(markup).toContain("died expect-timed-out at s6 waiting 200 to INVITE")
    expect(markup).toContain(`href="/api/triage/c.pcap/runs/run1051725/text?lane=fake&amp;case=c-case1"`)
  })
})
