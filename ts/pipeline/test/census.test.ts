/**
 * The census merge: which sweep hits become allowed-errors entries without
 * anyone being asked, and which are held for a human ruling.
 */
import type { AllowedErrors, Census } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import {
  describeHit,
  latestCensusReport,
  mergeCensus,
  renderRegistry,
  type CutOracle
} from "../src/census.js"
import { SutSet } from "../src/sut.js"
import { SOCKETS, SUT_ADDRESSES } from "./fixtures.js"

const CAPTURE = "race.pcap"
const CALL_ID = "some-call-id"

const noAckHit = (over: Partial<Census.NoAckHit> = {}): Census.CensusHit => ({
  rule: "no-ack-to-dialog-creating-2xx",
  document: "race.flows.json",
  capture: CAPTURE,
  group: 0,
  leg: 0,
  call_id: CALL_ID,
  emitter: SOCKETS.caller,
  emitter_role: "peer",
  taker: SOCKETS.sut,
  cseq: 1,
  relayed: false,
  final_msg: 3,
  final_hop: 0,
  final_ts_us: 4_000_000,
  to_tag: "sut-tag",
  status: 200,
  retransmits: 2,
  window_us: 10_000_000,
  emitter_window_us: 10_000_000,
  ...over
})

const report = (...hits: ReadonlyArray<Census.CensusHit>): Census.CensusReport => ({
  documents: 1,
  groups: 1,
  legs: 1,
  messages: hits.length,
  rules: {
    "no-200-after-cancel": tally(),
    "unacked-reliable-provisional": tally(),
    "no-ack-to-dialog-creating-2xx": tally(),
    "no-cancel-after-final": tally(),
    "second-answer-repeats-the-first": tally()
  },
  hits,
  failures: []
})

const tally = (): Census.RuleTally => ({
  hits: 0,
  documents: 0,
  occasions: 0,
  decided: 0,
  by_role: {},
  relayed: 0,
  buckets: {}
})

const EMPTY: AllowedErrors.AllowedErrors = { captures: {} }

/** The cut every capture below was taken with: the platform, and a case made. */
const cutTaken: CutOracle = () => ({ sut: new SutSet(SUT_ADDRESSES) })

describe("a source-side emitter", () => {
  it("is added mechanically, with a note that says a tool wrote it", () => {
    const merged = mergeCensus(EMPTY, report(noAckHit()), cutTaken)
    expect(merged.added).toHaveLength(1)
    expect(merged.needsRuling).toEqual([])
    const entries = merged.registry.captures[CAPTURE]!
    expect(entries).toHaveLength(1)
    expect(entries[0]!.originator).toBe(SOCKETS.caller)
    expect(entries[0]!.call_ids).toEqual([CALL_ID])
    expect(entries[0]!.note).toContain("Auto-added")
    expect(merged.added[0]!.side).toBe("peer")
  })

  it("is a no-op the second time, so the merge is idempotent", () => {
    const once = mergeCensus(EMPTY, report(noAckHit()), cutTaken)
    const twice = mergeCensus(once.registry, report(noAckHit()), cutTaken)
    expect(twice.added).toEqual([])
    expect(twice.duplicates).toHaveLength(1)
    expect(twice.registry).toEqual(once.registry)
  })

  it("gets its own entry for a new call-id rather than widening an existing one", () => {
    const once = mergeCensus(EMPTY, report(noAckHit()), cutTaken)
    const twice = mergeCensus(
      once.registry,
      report(noAckHit({ call_id: "another-call-id" })),
      cutTaken
    )
    const entries = twice.registry.captures[CAPTURE]!
    expect(entries).toHaveLength(2)
    expect(entries.map((e) => e.call_ids)).toEqual([[CALL_ID], ["another-call-id"]])
  })
})

describe("a hit the registry must never answer on its own", () => {
  it("holds a SUT-emitted violation for a human, and adds nothing", () => {
    const merged = mergeCensus(EMPTY, report(noAckHit({ emitter: SOCKETS.sut })), cutTaken)
    expect(merged.added).toEqual([])
    expect(merged.needsRuling).toHaveLength(1)
    expect(merged.needsRuling[0]!.side).toBe("platform")
    expect(merged.needsRuling[0]!.reason).toContain("never auto-accepted")
    // A capture whose every hit is refused must not gain an empty entry list and
    // read as a capture someone cleared.
    expect(merged.registry.captures[CAPTURE]).toBeUndefined()
  })

  it("holds a hit no deployment could place, because that is not a side", () => {
    const merged = mergeCensus(EMPTY, report(noAckHit()), () => ({}))
    expect(merged.needsRuling).toHaveLength(1)
    expect(merged.needsRuling[0]!.side).toBe("unattributed")
    expect(merged.needsRuling[0]!.reason).toContain("neither side")
  })
})

describe("what the cut settles outright", () => {
  it("reports each verdict in its own block, and never as a ruling owed", () => {
    const outcomes = ["outside-cut", "excluded-sut-invalid", "declared-negative"] as const
    for (const outcome of outcomes) {
      const merged = mergeCensus(EMPTY, report(noAckHit()), () => ({
        sut: new SutSet(SUT_ADDRESSES),
        verdict: { outcome, reason: `the cut says ${outcome}` }
      }))
      expect(merged.added).toEqual([])
      expect(merged.needsRuling).toEqual([])
      expect(merged.decisions[0]!.outcome).toBe(outcome)
      expect(merged.registry).toEqual(EMPTY)
    }
  })

  it("is tested BEFORE the duplicate check, so a stale entry still says so", () => {
    const once = mergeCensus(EMPTY, report(noAckHit()), cutTaken)
    const again = mergeCensus(once.registry, report(noAckHit()), () => ({
      sut: new SutSet(SUT_ADDRESSES),
      verdict: { outcome: "outside-cut", reason: "the cut no longer reaches this call" }
    }))
    expect(again.duplicates).toEqual([])
    expect(again.outsideCut).toHaveLength(1)
  })
})

describe("what a human reads", () => {
  it("describes each rule in the report's own numbers", () => {
    expect(describeHit(noAckHit())).toContain("never ACKed it")
    expect(describeHit(noAckHit())).toContain("through 2 retransmission(s)")
    expect(describeHit(noAckHit({ retransmits: 0 }))).not.toContain("retransmission(s)")
  })

  it("writes the registry back sorted, so a re-run diffs to nothing", () => {
    const registry: AllowedErrors.AllowedErrors = {
      captures: { "z.pcap": [], "a.pcap": [] }
    }
    const text = renderRegistry(registry)
    expect(text.indexOf("a.pcap")).toBeLessThan(text.indexOf("z.pcap"))
    expect(text.endsWith("\n")).toBe(true)
  })
})

describe("which sweep a consumer reads when none is named", () => {
  it("is the latest dated report, and nothing where none is dated", () => {
    const files = ["2026-08-23.census.json", "2026-08-23b.census.json", "notes.md"]
    expect(latestCensusReport(files)).toBe("2026-08-23b.census.json")
    expect(latestCensusReport(["notes.md"])).toBeUndefined()
  })
})
