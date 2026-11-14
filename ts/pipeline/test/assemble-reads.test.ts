/**
 * What assembling a case reads of its capture, never on a clock: each of the
 * case's messages a bounded number of times however long the call, and
 * nothing of another case's legs once the capture's own indexes are built. The
 * reads are counted at the source (`./counted-work.ts`).
 */
import { describe, expect, it } from "vitest"
import { assemble } from "../src/assemble.js"
import { captureIndex } from "../src/capture-index.js"
import type { CaseSpec } from "../src/case-spec.js"
import type { CallIdDerivation } from "../src/derivation.js"
import { policyWith } from "../src/policy.js"
import { scenario, SDP, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { plan } from "./fixtures.js"
import { watched, type Watched } from "./counted-work.js"

const derives: CallIdDerivation = (base, derived) => derived === `out-${base}`

/** One leg of a relayed call: opened, answered, `exchanges` in-dialog INFOs each retransmitted once, closed. */
const answered = (leg: ScenarioLeg, opener: "party" | "system", exchanges: number, at: number): ScenarioLeg => {
  leg
    .invite({ at, by: opener, body: SDP })
    .response(100, { at: at + 1 })
    .response(180, { at: at + 20 })
    .response(200, { at: at + 50, body: SDP })
    .ack({ at: at + 60, by: opener })
  for (let n = 0; n < exchanges; n++) {
    const sent = at + 100 + n * 10
    leg
      .request("INFO", { at: sent, by: opener })
      .request("INFO", { at: sent + 1, by: opener, retx: true })
      .response(200, { at: sent + 2 })
  }
  const end = at + 200 + exchanges * 10
  return leg.bye({ at: end, by: opener }).response(200, { at: end + 1 })
}

/** A caller's leg and the leg the system relays it onto, as one case cuts them. */
const call = (s: Scenario, name: string, exchanges: number, at: number): CaseSpec => {
  const caller = answered(s.leg({ callId: `${name}@example.com`, party: "198.51.100.10:5060" }), "party", exchanges, at)
  const callee = answered(s.leg({ callId: `out-${name}@example.com`, party: "203.0.113.20:5060" }), "system", exchanges, at + 5)
  s.group(caller, callee)
  return { id: name, uac: caller.vantage(), uas: [callee.vantage()], defects: [], cutLegs: [caller.index, callee.index] }
}

/** The capture of two such calls, its index built as the cut builds it, once. */
const twoCalls = (exchanges: number) => {
  const s = scenario()
  const first = call(s, "first", exchanges, 0)
  const second = call(s, "second", exchanges, 100_000)
  const capture = watched(s.flows())
  const index = captureIndex(capture.flows, derives, plan())
  return { s, first, second, capture, index }
}

const assembled = (t: ReturnType<typeof twoCalls>, spec: CaseSpec) =>
  assemble({
    flows: t.capture.flows,
    capture: "two-calls.pcap",
    spec,
    sut: t.s.sut,
    plan: plan(),
    policy: policyWith({ derives }),
    index: t.index
  })

/** The reads of each message of `legs` that `work` makes. */
const readsDuring = (capture: Watched, legs: ReadonlyArray<number>, work: () => unknown): ReadonlyArray<number> => {
  const before = legs.map((leg) => capture.readsOf(leg))
  work()
  return legs.flatMap((leg, i) => capture.readsOf(leg).map((n, msg) => n - before[i]![msg]!))
}

/** The most any message of the first case was read while assembling it. */
const mostReadOfFirst = (exchanges: number): number => {
  const t = twoCalls(exchanges)
  let steps = 0
  const reads = readsDuring(t.capture, t.first.cutLegs!, () => {
    steps = assembled(t, t.first).pivot.flow.length
  })
  expect(steps).toBe(2 * (7 + 2 * exchanges))
  expect(reads.every((n) => n > 0)).toBe(true)
  return Math.max(...reads)
}

describe("assembling a case", () => {
  it("reads each of its messages a bounded number of times, however long the call", () => {
    expect(mostReadOfFirst(400)).toBe(mostReadOfFirst(25))
  })

  it("reads nothing of another case's legs once the capture's indexes are built", () => {
    const t = twoCalls(25)
    assembled(t, t.first)
    const reads = readsDuring(t.capture, t.first.cutLegs!, () => assembled(t, t.second))

    expect(reads.filter((n) => n > 0)).toEqual([])
  })
})
