/**
 * What call facts cost, counted in the module's own visits and never on a
 * clock: linear in a position's messages, blind to the legs a position does
 * not own, and free on a second request.
 */
import { describe, expect, it } from "vitest"
import * as CallFacts from "../src/call-facts.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"

const PARTY = "198.51.100.10:5060"

/** An answered leg holding `exchanges` accepted in-dialog requests, each retransmitted once. */
const longCall = (s: Scenario, exchanges: number): ScenarioLeg => {
  const leg = s
    .leg({ callId: "long@example.com", party: PARTY })
    .invite({ at: 0, by: "party" })
    .response(180, { at: 10 })
    .response(200, { at: 20 })
    .ack({ at: 30, by: "party" })
  for (let n = 0; n < exchanges; n++) {
    const at = 100 + n * 10
    const by = n % 2 === 0 ? "party" : "system"
    leg.request("INFO", { at, by }).request("INFO", { at: at + 1, by, retx: true }).response(200, { at: at + 2 })
  }
  return leg.bye({ at: 100 + exchanges * 10, by: "party" }).response(200, { at: 101 + exchanges * 10 })
}

const decoys = (s: Scenario, count: number): void => {
  for (let n = 0; n < count; n++) {
    s.leg({ callId: `decoy-${n}@example.com`, party: `203.0.113.${1 + (n % 200)}:5060` })
      .invite({ at: n, by: "party" })
      .response(486, { at: n + 1 })
      .ack({ at: n + 2, by: "party" })
  }
}

const visitsFor = (exchanges: number, decoyLegs = 0): number => {
  const s = scenario()
  const leg = longCall(s, exchanges)
  decoys(s, decoyLegs)
  const flows = s.flows()
  const read = CallFacts.atVantage(flows, s.sut, leg.vantage())
  expect(read.accepted).toHaveLength(exchanges + 1)
  return CallFacts.cost(flows).visits
}

describe("the cost of a position's facts", () => {
  it("at most 2.2 times the visits for twice the messages", () => {
    const once = visitsFor(400)
    const twice = visitsFor(800)

    expect(once).toBeGreaterThan(400)
    expect(twice).toBeLessThanOrEqual(2.2 * once)
  })

  it("is the same whatever other legs the file holds", () => {
    expect(visitsFor(400, 300)).toBe(visitsFor(400))
  })

  it("is nothing on a second request for the same position", () => {
    const s = scenario()
    const leg = longCall(s, 400)
    const flows = s.flows()
    const first = CallFacts.atVantage(flows, s.sut, leg.vantage())
    const paid = CallFacts.cost(flows)

    expect(CallFacts.atVantage(flows, s.sut, leg.vantage())).toBe(first)
    expect(CallFacts.legFacts(CallFacts.ofVantage(flows, s.sut, leg.vantage()))).toBe(first)
    expect(CallFacts.cost(flows)).toEqual(paid)
  })

  it("reads the capture's end once per file, one message a leg", () => {
    const s = scenario()
    const cut = s.leg({ callId: "cut@example.com", party: PARTY }).invite({ at: 0, by: "party" })
    const other = s.leg({ callId: "cut-too@example.com", party: PARTY }).invite({ at: 1, by: "party" })
    decoys(s, 50)
    const flows = s.flows()

    expect(CallFacts.atVantage(flows, s.sut, cut.vantage()).final.state).toBe("not-captured")
    expect(CallFacts.atVantage(flows, s.sut, other.vantage()).final.state).toBe("not-captured")
    expect(CallFacts.cost(flows).indexed).toBe(52)
  })
})
