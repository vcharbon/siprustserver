/**
 * The queue's ruling: live run status crossed with the verdict registry.
 *
 * Driven through `overlay` rather than its private helpers, because the whole
 * point of the pure function is that the sweep and the viewer get the same
 * answer from the same three inputs.
 */
import type * as Triage from "../src/registry.js"
import { describe, expect, it } from "vitest"
import type { Standing } from "../src/model.js"
import { NO_DELTAS } from "../src/model.js"
import { inScope, knownCaptures, overlay } from "../src/queue.js"

const standing = (over: Partial<Standing>): Standing => ({
  capture: "c.pcap.gz",
  lane: "fake",
  runId: 1,
  status: "done",
  prepId: 1,
  bundlePath: "/w/c/run1",
  finishedAt: "2026-08-25T10:00:00.000Z",
  cases: 1,
  failed: 0,
  skipped: 0,
  caseIds: ["c.pcap.gz-case1"],
  failedSteps: [],
  skipReasons: [],
  caseResults: [],
  rfcGating: 0,
  deltas: NO_DELTAS,
  lastOkRunId: null,
  lastOkBundlePath: null,
  caseRuns: [],
  ...over
})

const pending = (ticket: string): Triage.RegistryRecord => ({
  verdict: "bug",
  ticket,
  decided_on: "2026-08-25",
  evidence: { bundle: "w/c/run1/cell" }
})

const final: Triage.RegistryRecord = {
  verdict: "expected",
  decided_on: "2026-08-25",
  evidence: { bundle: "w/c/run1/cell" }
}

const NO_TICKETS: ReadonlyMap<string, Triage.TicketState> = new Map()

describe("overlay", () => {
  it("takes a failing capture with no record as untriaged", () => {
    const view = overlay({
      standings: [standing({ failed: 1 })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows.map((row) => row.status)).toEqual(["failing-untriaged"])
    expect(view.rows[0]!.cases).toEqual(["c.pcap.gz-case1"])
  })

  it("carries the run's failed steps and delta counts onto the row", () => {
    const view = overlay({
      standings: [standing({
        failed: 1,
        failedSteps: ["s25"],
        deltas: { ...NO_DELTAS, cases: 1, accepted: 77, records: 77, compared: 12 }
      })],
      registry: {},
      tickets: NO_TICKETS
    })
    // A clean delta sheet under a failing run: the counts rule on wire
    // DIFFERENCES, and a replay that stopped at s25 diverged from little.
    expect(view.rows[0]!.failedSteps).toEqual(["s25"])
    expect(view.rows[0]!.deltas.accepted).toBe(77)
    expect(view.rows[0]!.deltas.unknown).toBe(0)
    expect(view.rows[0]!.status).toBe("failing-untriaged")
  })

  it("drops a green capture nobody has a record for", () => {
    const view = overlay({ standings: [standing({})], registry: {}, tickets: NO_TICKETS })
    expect(view.rows).toEqual([])
  })

  it("suppresses a failure while its ticket is open, re-queues it once closed", () => {
    const registry = { "c.pcap.gz": pending("t.md") }
    const open = overlay({
      standings: [standing({ failed: 1 })],
      registry,
      tickets: new Map([["t.md", "open" as const]])
    })
    expect(open.rows[0]!.status).toBe("failing-suppressed")
    const closed = overlay({
      standings: [standing({ failed: 1 })],
      registry,
      tickets: new Map([["t.md", "closed" as const]])
    })
    expect(closed.rows[0]!.status).toBe("failing-requeue")
  })

  it("proposes closing a pending record whose capture now passes", () => {
    const view = overlay({
      standings: [standing({ lastOkRunId: 1, lastOkBundlePath: "/w/c/run1" })],
      registry: { "c.pcap.gz": pending("t.md") },
      tickets: new Map([["t.md", "open" as const]])
    })
    expect(view.rows[0]!.status).toBe("passing-but-pending")
    expect(view.rows[0]!.lastOkRunId).toBe(1)
  })

  it("keeps a failure under a final record visible, and off the takeable statuses", () => {
    const view = overlay({
      standings: [standing({ failed: 1 })],
      registry: { "c.pcap.gz": final },
      tickets: NO_TICKETS
    })
    expect(view.rows[0]!.status).toBe("failing-excluded")
    expect(view.offQueue).toEqual([])
  })

  it("never loses a record whose capture earns no row", () => {
    const view = overlay({ standings: [], registry: { "gone.pcap.gz": final }, tickets: NO_TICKETS })
    expect(view.rows).toEqual([])
    expect(view.offQueue.map((row) => row.capture)).toEqual(["gone.pcap.gz"])
  })

  it("counts a run the cut refused every case for as no-cases: visible, and not takeable", () => {
    const view = overlay({
      standings: [standing({ status: "no-cases", cases: 0, caseIds: [] })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows[0]!.status).toBe("no-cases")
    expect(view.totals["failing-untriaged"]).toBeUndefined()
  })

  it("keeps a refused cut off the failing statuses even under a registry record", () => {
    const view = overlay({
      standings: [standing({ status: "no-cases", cases: 0, caseIds: [] })],
      registry: { "c.pcap.gz": final },
      tickets: NO_TICKETS
    })
    // The record rules on a replay, and a refused cut produced none: the row
    // still carries the verdict, but it is not an excluded FAILURE.
    expect(view.rows[0]!.status).toBe("no-cases")
    expect(view.rows[0]!.verdict).toBe("expected")
  })

  it("counts a run whose only case the driver skipped as skipped, not failing", () => {
    const view = overlay({
      standings: [standing({ skipped: 1, skipReasons: ["blocked:claim-ambiguous"] })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows.map((row) => row.status)).toEqual(["skipped"])
    expect(view.rows[0]!.failed).toBe(0)
    expect(view.totals["failing-untriaged"]).toBeUndefined()
    // The reason rides the row verbatim: a skipped capture that cannot say why
    // is indistinguishable from one nobody ran.
    expect(view.rows[0]!.skipReasons).toEqual(["blocked:claim-ambiguous"])
  })

  it("carries each case of the run as itself, so a call is never read off an aggregate", () => {
    const view = overlay({
      standings: [standing({
        cases: 2,
        failed: 1,
        caseIds: ["c.pcap.gz-case1", "c.pcap.gz-case2"],
        failedSteps: ["s9"],
        caseResults: [
          { caseId: "c.pcap.gz-case1", passed: false, failedStep: "s9", skipped: null },
          { caseId: "c.pcap.gz-case2", passed: true, failedStep: null, skipped: null }
        ]
      })],
      registry: {},
      tickets: NO_TICKETS
    })
    // The flat list cannot say WHICH call died at s9; the per-case rows can.
    expect(view.rows[0]!.failedSteps).toEqual(["s9"])
    expect(view.rows[0]!.caseResults.map((entry) => [entry.caseId, entry.failedStep])).toEqual([
      ["c.pcap.gz-case1", "s9"],
      ["c.pcap.gz-case2", null]
    ])
  })

  it("carries each case's OWN last run onto the row, which may be a cases run the standing ignores", () => {
    // The standing is the full run (red); case1 was since replayed alone and
    // passed there. The row must say both: the capture is red, the CALL's last
    // word is green.
    const view = overlay({
      standings: [standing({
        runId: 7,
        cases: 2,
        failed: 2,
        caseIds: ["c.pcap.gz-case1", "c.pcap.gz-case2"],
        caseResults: [
          { caseId: "c.pcap.gz-case1", passed: false, failedStep: "s9", skipped: null },
          { caseId: "c.pcap.gz-case2", passed: false, failedStep: "s9", skipped: null }
        ],
        caseRuns: [
          { caseId: "c.pcap.gz-case1", runId: 8, setKind: "cases", passed: true, skipped: null, bundlePath: "/w/c/run8" },
          { caseId: "c.pcap.gz-case2", runId: 7, setKind: "full", passed: false, skipped: null, bundlePath: "/w/c/run7" }
        ]
      })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows[0]!.status).toBe("failing-untriaged")
    expect(view.rows[0]!.runId).toBe(7)
    expect(view.rows[0]!.caseRuns).toEqual([
      { caseId: "c.pcap.gz-case1", runId: 8, setKind: "cases", passed: true, skipped: null, bundlePath: "/w/c/run8" },
      { caseId: "c.pcap.gz-case2", runId: 7, setKind: "full", passed: false, skipped: null, bundlePath: "/w/c/run7" }
    ])
  })

  it("keeps a run failing when one case failed beside a skipped one", () => {
    const view = overlay({
      standings: [standing({
        cases: 2,
        failed: 1,
        skipped: 1,
        skipReasons: ["blocked:claim-ambiguous"],
        caseIds: ["c.pcap.gz-case1", "c.pcap.gz-case2"]
      })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows[0]!.status).toBe("failing-untriaged")
  })

  it("keeps a run green when a case passed beside a skipped one", () => {
    const view = overlay({
      standings: [standing({ cases: 2, skipped: 1, skipReasons: ["blocked:claim-ambiguous"] })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(view.rows).toEqual([])
  })

  it("reports the universe entries with no run as never-run", () => {
    const view = overlay({
      standings: [standing({ failed: 1 })],
      registry: {},
      tickets: NO_TICKETS,
      captures: ["c.pcap.gz", "other.pcap.gz"]
    })
    expect(view.neverRun).toEqual(["other.pcap.gz"])
  })

  it("orders untriaged before requeue before suppressed", () => {
    const view = overlay({
      standings: [
        standing({ capture: "a.pcap.gz", failed: 1 }),
        standing({ capture: "b.pcap.gz", failed: 1 }),
        standing({ capture: "d.pcap.gz", failed: 1 })
      ],
      registry: { "a.pcap.gz": pending("open.md"), "b.pcap.gz": pending("closed.md") },
      tickets: new Map([["open.md", "open" as const], ["closed.md", "closed" as const]])
    })
    expect(view.rows.map((row) => row.capture)).toEqual(["d.pcap.gz", "b.pcap.gz", "a.pcap.gz"])
  })
})

describe("knownCaptures", () => {
  it("leaves out a capture every case of which was skipped", () => {
    const list = knownCaptures({
      standings: [standing({ capture: "blocked.pcap.gz", skipped: 1, skipReasons: ["blocked:x"] })],
      registry: {},
      tickets: NO_TICKETS
    })
    expect(list).toEqual([])
  })

  it("is every pending record plus every current failure, deduped", () => {
    const list = knownCaptures({
      standings: [standing({ capture: "c.pcap.gz", failed: 1 }), standing({ capture: "green.pcap.gz" })],
      registry: { "c.pcap.gz": pending("t.md"), "settled.pcap.gz": final, "quiet.pcap.gz": pending("u.md") },
      tickets: NO_TICKETS
    })
    expect(list).toEqual(["c.pcap.gz", "quiet.pcap.gz"])
  })
})

describe("inScope", () => {
  /**
   * The lane whole: a green row, a red one, a cut the CUT refused every call
   * of, and a run the driver crashed out of before it ran a case: that one
   * has calls to look at and is triage work, whatever its case count says.
   */
  const lane = overlay({
    standings: [
      standing({ capture: "green.pcap.gz" }),
      standing({ capture: "red.pcap.gz", failed: 1 }),
      standing({ capture: "refused.pcap.gz", status: "no-cases", cases: 0, caseIds: [] }),
      standing({ capture: "empty.pcap.gz", status: "error", cases: 0, caseIds: [] })
    ],
    registry: {},
    tickets: NO_TICKETS,
    includePassing: true
  })

  it("keeps a refused cut off `all`, green and errored runs included, and the totals still count it", () => {
    const rows = lane.rows.filter(inScope("all"))
    expect(rows.map((row) => row.capture).sort()).toEqual(["empty.pcap.gz", "green.pcap.gz", "red.pcap.gz"])
    expect(lane.totals["no-cases"]).toBe(1)
  })

  it("keeps the takeable statuses under `takeable`, an errored run among them", () => {
    const rows = lane.rows.filter(inScope("takeable"))
    expect(rows.map((row) => [row.capture, row.status])).toEqual([
      ["empty.pcap.gz", "failing-untriaged"],
      ["red.pcap.gz", "failing-untriaged"]
    ])
  })

  it("serves the refused cuts, and only them, under the explicit `no-cases` scope", () => {
    const rows = lane.rows.filter(inScope("no-cases"))
    expect(rows.map((row) => row.capture)).toEqual(["refused.pcap.gz"])
  })
})
