/**
 * One file of the Pcaps page off its stat and its source row: a file written
 * after its row's last ingest is a file the row does not describe, so it lists
 * as `new`, without the earlier file's ingest, cut, cases, problem or runs.
 */
import type { Sources } from "@sip/store"
import { describe, expect, it } from "vitest"
import { pcapOf } from "../src/pcaps.ts"

const filePath = "/ws/pcaps/capture-a.anon.pcap"

const failedCut: Sources.Row = {
  capture: "capture-a",
  sourcePath: filePath,
  addedAt: "2026-10-01T09:00:00.000Z",
  ingestedAt: "2026-10-01T09:00:05.000Z",
  extractorHash: "old-extractor",
  ingest: "stale",
  ingestFailed: "pcap-anon auto failed: 1 names survive",
  prepId: 8,
  cutAt: "2026-10-01T09:01:00.000Z",
  cut: "stale",
  cutBroke: null,
  cases: 0,
  refusals: 0,
  spanSeconds: null,
  standings: [{
    lane: "simulated",
    runId: 4,
    status: "error",
    cases: 0,
    failed: 0,
    skipped: 0,
    finishedAt: "2026-10-01T09:02:00.000Z"
  }]
}

const stat = (modifiedAt: string) => ({
  name: "capture-a.anon.pcap",
  path: filePath,
  size: 2048,
  mtimeMs: Date.parse(modifiedAt)
})

describe("a file of the pcaps page", () => {
  it("written after its row's last ingest lists as new, with nothing of the earlier file's", () => {
    expect(pcapOf(stat("2026-11-12T10:00:00.000Z"), failedCut)).toEqual({
      file: "capture-a.anon.pcap",
      size: 2048,
      modifiedAt: "2026-11-12T10:00:00.000Z",
      capture: "capture-a",
      state: "new",
      cases: null,
      ingest: null,
      cut: null,
      problem: null,
      standings: []
    })
  })

  it("written before its row's last ingest keeps that ingest's cut, problem and runs", () => {
    const listed = pcapOf(stat("2026-10-01T08:59:00.000Z"), failedCut)
    expect(listed.state).toBe("cut")
    expect(listed.cases).toBe(0)
    expect(listed.ingest).toBe("stale")
    expect(listed.cut).toBe("stale")
    expect(listed.problem).toBe("pcap-anon auto failed: 1 names survive")
    expect(listed.standings).toHaveLength(1)
  })

  it("never ingested and written after the row was added lists as new", () => {
    const added = { ...failedCut, ingestedAt: null, ingestFailed: null, prepId: null, cutAt: null, cut: "never" } as const
    expect(pcapOf(stat("2026-10-02T00:00:00.000Z"), added).state).toBe("new")
  })
})
