/**
 * What call facts cost, never on a clock: linear in a position's messages,
 * blind to the legs a position does not own, and free on a second request.
 * The work is counted at the source (`./counted-work.ts`): every read of a
 * message of the document and every array element stepped over, added to the
 * visits the module reports of itself.
 */
import { describe, expect, it } from "vitest"
import * as CallFacts from "../src/call-facts.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { arraySteps, watched } from "./counted-work.js"

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

/** The work of reading the long call's facts at its vantage: message reads, array steps and reported visits. */
const workFor = (exchanges: number, decoyLegs = 0): number => {
  const s = scenario()
  const leg = longCall(s, exchanges)
  decoys(s, decoyLegs)
  const { flows, reads } = watched(s.flows())
  const { result, steps } = arraySteps(() => CallFacts.atVantage(flows, s.sut, leg.vantage()))
  expect(result.accepted).toHaveLength(exchanges + 1)
  return reads() + steps + CallFacts.cost(flows).visits
}

describe("the cost of a position's facts", () => {
  it("at most 2.2 times the work for twice the messages", () => {
    const once = workFor(400)
    const twice = workFor(800)

    expect(once).toBeGreaterThan(400)
    expect(twice).toBeLessThanOrEqual(2.2 * once)
  })

  it("is the same whatever other legs the file holds", () => {
    expect(workFor(400, 300)).toBe(workFor(400))
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

/**
 * A caller's leg holding `exchanges` accepted in-dialog requests, and
 * `attempts` legs the system dials and cancels one after the other, the first
 * a second after the caller's BYE.
 */
const hunt = (s: Scenario, attempts: number, exchanges: number): { readonly caller: ScenarioLeg; readonly chain: ReadonlyArray<ScenarioLeg> } => {
  const caller = longCall(s, exchanges)
  const first = 1_100 + exchanges * 10
  const chain = Array.from({ length: attempts }, (_, n) => {
    const at = first + 1_000 * n
    return s
      .leg({ callId: `attempt-${n}@example.com`, party: `203.0.113.${1 + (n % 200)}:5060` })
      .invite({ at, by: "system" })
      .response(180, { at: at + 30 })
      .cancel({ at: at + 900, by: "system" })
      .response(200, { at: at + 905 })
      .response(487, { at: at + 910, to: "INVITE" })
      .ack({ at: at + 915, by: "system" })
  })
  return { caller, chain }
}

/** The work of the call level over a hunt whose legs' facts are already kept: message reads and array steps. */
const callWorkFor = (attempts: number, exchanges: number): { readonly reads: number; readonly steps: number } => {
  const s = scenario()
  const { caller, chain } = hunt(s, attempts, exchanges)
  const { flows, reads } = watched(s.flows())
  const at = (leg: ScenarioLeg) => CallFacts.atVantage(flows, s.sut, leg.vantage())
  const legs = { caller: [at(caller)], chain: [chain.map(at)] }
  const before = reads()
  const { result, steps } = arraySteps(() => {
    const call = CallFacts.ofCall(legs)
    return legs.chain[0]!.map((leg) => [CallFacts.release(call, leg, 500_000).kind, CallFacts.ringTime(leg).end.state])
  })
  expect(result).toEqual(chain.map(() => ["own", "seen"]))
  return { reads: reads() - before, steps }
}

describe("the cost of a call's facts over legs already read", () => {
  it("reads no message", () => {
    expect(callWorkFor(50, 400).reads).toBe(0)
  })

  it("at most 2.2 times the steps for twice the attempts", () => {
    const once = callWorkFor(50, 10).steps

    expect(once).toBeGreaterThan(50)
    expect(callWorkFor(100, 10).steps).toBeLessThanOrEqual(2.2 * once)
  })

  it("does not grow with the messages of the caller's leg", () => {
    expect(callWorkFor(50, 800).steps).toBe(callWorkFor(50, 10).steps)
  })
})

/** The work of merging a hunt's positions whose facts are already kept: message reads and array steps. */
const mergeWorkFor = (attempts: number, exchanges: number): { readonly reads: number; readonly steps: number } => {
  const s = scenario()
  const { caller, chain } = hunt(s, attempts, exchanges)
  const { flows, reads } = watched(s.flows())
  const timelines = [caller, ...chain].map((leg) => CallFacts.atVantage(flows, s.sut, leg.vantage()).timeline)
  const before = reads()
  const { result, steps } = arraySteps(() => CallFacts.merged(timelines))
  expect(result).toHaveLength(6 * attempts + 6 + 3 * exchanges)
  return { reads: reads() - before, steps }
}

describe("the cost of merging positions already read", () => {
  it("reads no message", () => {
    expect(mergeWorkFor(50, 400).reads).toBe(0)
  })

  it("at most 2.2 times the steps for twice the messages", () => {
    const once = mergeWorkFor(50, 400).steps

    expect(once).toBeGreaterThan(400)
    expect(mergeWorkFor(100, 800).steps).toBeLessThanOrEqual(2.2 * once)
  })
})

describe("the cost of the party tags", () => {
  it("is one read of each message of the file, however many tags are asked for", () => {
    const s = scenario()
    decoys(s, 50)
    const flows = s.flows()
    const tags = CallFacts.partyTags(flows, s.sut)
    const paid = CallFacts.cost(flows)

    expect(paid).toEqual({ visits: 0, indexed: 150 })
    for (let n = 0; n < 1000; n++) tags.onAnotherLeg(`tag-${n}`, n % 50)
    expect(CallFacts.partyTags(flows, s.sut)).toBe(tags)
    expect(CallFacts.cost(flows)).toEqual(paid)
  })
})
