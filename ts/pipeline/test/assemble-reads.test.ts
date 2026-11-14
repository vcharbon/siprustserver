/**
 * What assembling a case costs of its capture, never on a clock: each of the
 * case's messages read a bounded number of times however long the call,
 * nothing of another case's legs once the capture's own indexes are built,
 * and array work linear in the call and blind to the capture's other calls.
 * The reads and the array steps are counted at the source (`./counted-work.ts`).
 */
import { describe, expect, it } from "vitest"
import { assemble } from "../src/assemble.js"
import { captureIndex } from "../src/capture-index.js"
import type { CaseSpec } from "../src/case-spec.js"
import type { CallIdDerivation } from "../src/derivation.js"
import { policyWith } from "../src/policy.js"
import { scenario, SDP, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { plan } from "./fixtures.js"
import { arraySteps, watched, type Watched } from "./counted-work.js"

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

/**
 * The capture of a first call of `exchanges` and `others` calls of 25 after it,
 * its index built as the cut builds it, once.
 */
const capture = (exchanges: number, others = 1) => {
  const s = scenario()
  const first = call(s, "first", exchanges, 0)
  const rest = Array.from({ length: others }, (_, n) => call(s, `other-${n}`, 25, 100_000 * (n + 1)))
  const watching = watched(s.flows())
  const index = captureIndex(watching.flows, derives, plan())
  return { s, first, second: rest[0]!, capture: watching, index }
}

type Capture = ReturnType<typeof capture>

const assembled = (t: Capture, spec: CaseSpec) =>
  assemble({
    flows: t.capture.flows,
    capture: "calls.pcap",
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
  const t = capture(exchanges)
  let steps = 0
  const reads = readsDuring(t.capture, t.first.cutLegs!, () => {
    steps = assembled(t, t.first).pivot.flow.length
  })
  expect(steps).toBe(2 * (7 + 2 * exchanges))
  expect(reads.every((n) => n > 0)).toBe(true)
  return Math.max(...reads)
}

/**
 * The array elements stepped over while assembling the first call, once a case
 * of the same capture was assembled: what one case costs past the file's own
 * work, which the first case of a file pays.
 */
const stepsOfFirst = (exchanges: number, others = 1): number => {
  const t = capture(exchanges, others)
  assembled(t, t.second)
  const { result, steps } = arraySteps(() => assembled(t, t.first))
  expect(result.pivot.flow).toHaveLength(2 * (7 + 2 * exchanges))
  return steps
}

describe("assembling a case", () => {
  it("reads each of its messages a bounded number of times, however long the call", () => {
    expect(mostReadOfFirst(400)).toBe(mostReadOfFirst(25))
  })

  it("reads nothing of another case's legs once the capture's indexes are built", () => {
    const t = capture(25)
    assembled(t, t.first)
    const reads = readsDuring(t.capture, t.first.cutLegs!, () => assembled(t, t.second))

    expect(reads.filter((n) => n > 0)).toEqual([])
  })

  it("steps over at most 2.2 times the array elements for a call twice as long: no pass is quadratic in the call", () => {
    const once = stepsOfFirst(200)

    expect(once).toBeGreaterThan(2 * (7 + 2 * 200))
    expect(stepsOfFirst(400)).toBeLessThanOrEqual(2.2 * once)
  })

  it("steps over as many array elements however many other calls the capture holds", () => {
    expect(stepsOfFirst(25, 20)).toBe(stepsOfFirst(25))
  })
})
