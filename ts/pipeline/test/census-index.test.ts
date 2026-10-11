/**
 * A census report's hits indexed once by capture and Call-ID: a lookup answers
 * exactly the hits a filter of the whole report answers, in report order, and a
 * reader asking about many cases reads each hit a bounded number of times
 * rather than once per case.
 */
import type { Census } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { capturedWith } from "../src/captured.js"
import { indexHits } from "../src/census-index.js"
import { caseCallIds, correlate } from "../src/cut.js"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  cancelRaceFlows,
  derivesOnePrefix,
  plan,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const CAPTURES = 12
const CALLS_PER_CAPTURE = 400

const captureName = (c: number): string => `capture-${c}.pcap`
const callOf = (c: number, k: number): string => `call-${c}-${k}@host.example`

/** `no-200-after-cancel` on the callee leg's 200 (msg 5) of {@link cancelRaceFlows}, overridden. */
const answeredAfterCancel = (over: Partial<Census.CensusHit>): Census.CensusHit =>
  ({
    document: "race.flows.json",
    capture: captureName(0),
    rule: "no-200-after-cancel",
    group: 0,
    leg: 1,
    call_id: CALLEE_CALL_ID,
    emitter: SOCKETS.callee,
    emitter_role: "peer",
    taker: SOCKETS.sut,
    cseq: 1,
    relayed: false,
    cancel_msg: 3,
    cancel_hop: 0,
    cancel_ts_us: 1_020_000,
    response_msg: 5,
    response_hop: 0,
    response_ts_us: 1_060_000,
    status: 200,
    gap_us: 40_000,
    ...over
  }) as Census.CensusHit

/**
 * Hits interleaved across captures as a sweep writes them, one or two per call,
 * with the fixture's own two calls carried by every capture.
 */
const syntheticCensus = (): ReadonlyArray<Census.CensusHit> => {
  const hits: Array<Census.CensusHit> = []
  for (let k = 0; k < CALLS_PER_CAPTURE; k++) {
    for (let c = 0; c < CAPTURES; c++) {
      const capture = captureName(c)
      if (k === 0) {
        hits.push(answeredAfterCancel({ capture }))
        hits.push(answeredAfterCancel({ capture, leg: 0, call_id: CALLER_CALL_ID }))
      }
      hits.push(answeredAfterCancel({ capture, call_id: callOf(c, k) }))
      if (k % 3 === 0) hits.push(answeredAfterCancel({ capture, call_id: callOf(c, k), cseq: 2 }))
    }
  }
  return hits
}

/** One case: a capture and the Call-IDs it carries, a repeated one and one no hit names included. */
interface OneCase {
  readonly capture: string
  readonly callIds: ReadonlyArray<string>
}

const cases = (): ReadonlyArray<OneCase> => {
  const out: Array<OneCase> = []
  for (let c = 0; c < CAPTURES; c++) {
    for (let n = 0; n < 20; n++) {
      const k = (n * 17) % CALLS_PER_CAPTURE
      out.push({
        capture: captureName(c),
        callIds: [callOf(c, k + 1), CALLER_CALL_ID, CALLEE_CALL_ID, callOf(c, k), callOf(c, k + 1), "absent@x"]
      })
    }
  }
  out.push({ capture: "never-swept.pcap", callIds: [CALLER_CALL_ID] })
  return out
}

/** The reference: every hit of the case's capture on one of its calls, by a walk of the whole report. */
const filtered = (hits: ReadonlyArray<Census.CensusHit>, one: OneCase): ReadonlyArray<Census.CensusHit> => {
  const wanted = new Set(one.callIds)
  return hits.filter((h) => h.capture === one.capture && wanted.has(h.call_id))
}

/** The hits, each behind a proxy counting every property read. */
const counted = (hits: ReadonlyArray<Census.CensusHit>) => {
  let reads = 0
  const proxied = hits.map(
    (hit) =>
      new Proxy(hit, {
        get: (target, key, receiver) => {
          reads += 1
          return Reflect.get(target, key, receiver)
        }
      })
  )
  return { hits: proxied, reads: () => reads }
}

describe("the census hit index", () => {
  const hits = syntheticCensus()

  it("answers a case's calls with exactly the hits a walk of the report answers, in report order", () => {
    const index = indexHits(hits)
    for (const one of cases()) {
      expect(index.ofCalls(one.capture, one.callIds)).toEqual(filtered(hits, one))
      for (const callId of one.callIds) {
        expect(index.ofCall(one.capture, callId)).toEqual(filtered(hits, { capture: one.capture, callIds: [callId] }))
      }
    }
  })

  it("answers a capture with its hits in report order, and an unswept capture with none", () => {
    const index = indexHits(hits)
    expect(index.ofCapture(captureName(3))).toEqual(hits.filter((h) => h.capture === captureName(3)))
    expect(index.ofCapture("never-swept.pcap")).toEqual([])
  })

  it("states each case's captured violations off its own hits, never a walk of the report", () => {
    const flows = cancelRaceFlows()
    const sut = sutSet()
    const layout = build(flows, [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }], sut, plan(), derivesOnePrefix)
    const flow = synthesize(flows, layout, plan())
    const own = caseCallIds(flows, sut, [0, 1], correlate(flows, derivesOnePrefix))
    const inputOf = (one: OneCase) => ({
      layout,
      capture: one.capture,
      callIds: [...own, ...one.callIds],
      flows,
      sources: flow.sources,
      steps: flow.steps
    })

    const probe = counted(hits)
    const stating = capturedWith(indexHits(probe.hits))
    const all = cases()
    let mine = 0
    for (const one of all) {
      const input = inputOf(one)
      const got = stating(input)
      mine += filtered(hits, { capture: one.capture, callIds: input.callIds }).length
      expect(got).toEqual(capturedWith(indexHits(filtered(hits, { capture: one.capture, callIds: input.callIds })))(input))
    }
    const bound = 4 * hits.length + 40 * mine
    // A walk of the whole report per case reads at least hits × cases.
    expect(5 * bound).toBeLessThan(hits.length * all.length)
    expect(probe.reads()).toBeLessThanOrEqual(bound)
  })
})
