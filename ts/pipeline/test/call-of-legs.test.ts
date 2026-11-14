/**
 * Call facts over legs: the caller's answer, failure and give-up, an attempt's
 * ring time, and whether the system released a dialled leg on its own. The
 * test names the legs of each call, as a rule does.
 */
import { describe, expect, it } from "vitest"
import * as CallFacts from "../src/call-facts.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"

const CALLER = "198.51.100.10:5060"
const CALLEE = "203.0.113.20:5060"
const SKEW_US = 500_000

const caller = (s: Scenario): ScenarioLeg =>
  s.leg({ callId: "caller@example.com", party: CALLER }).invite({ at: 0, by: "party" }).response(100, { at: 5 })

/** A leg the system dials at `at` ms, which rings 30 ms later where `rings`. */
const attempt = (s: Scenario, at: number, rings = true): ScenarioLeg => {
  const leg = s.leg({ callId: `attempt-${at}@example.com`, party: CALLEE }).invite({ at, by: "system" })
  return rings ? leg.response(180, { at: at + 30 }) : leg
}

const cancelled = (leg: ScenarioLeg, at: number): ScenarioLeg =>
  leg.cancel({ at, by: "system" }).response(200, { at: at + 5 }).response(487, { at: at + 20, to: "INVITE" }).ack({ at: at + 30, by: "system" })

const callOf = (s: Scenario, callerLeg: ScenarioLeg, ...chain: ReadonlyArray<ScenarioLeg>) => {
  const flows = s.flows()
  const at = (leg: ScenarioLeg) => CallFacts.atVantage(flows, s.sut, leg.vantage())
  const legs = chain.map(at)
  return { call: CallFacts.ofCall({ caller: [at(callerLeg)], chain: [legs] }), legs }
}

describe("the caller", () => {
  it("was answered at the 2xx to its INVITE, which a later re-INVITE's 2xx does not move", () => {
    const s = scenario()
    const a = caller(s)
      .response(200, { at: 900 })
      .ack({ at: 910, by: "party" })
      .invite({ at: 2_000, by: "system" })
      .response(200, { at: 2_020 })
      .ack({ at: 2_030, by: "system" })
    const { call } = callOf(s, a)

    expect(CallFacts.valueOf(call.answered)?.at_us).toBe(900_000)
    expect(call.failed.state).toBe("absent")
    expect(call.gaveUp.state).toBe("not-captured")
  })

  it("got a failure final where the system answered its INVITE with one", () => {
    const s = scenario()
    const a = caller(s).response(480, { at: 700 }).ack({ at: 710, by: "party" })
    const { call } = callOf(s, a)

    expect(CallFacts.valueOf(call.failed)).toMatchObject({ status: 480, at_us: 700_000 })
    expect(call.answered.state).toBe("absent")
    expect(call.gaveUp.state).toBe("absent")
  })

  it("gave up at its CANCEL (RFC 3261 §9.1)", () => {
    const s = scenario()
    const a = caller(s)
      .cancel({ at: 4_000, by: "party" })
      .response(200, { at: 4_005 })
      .response(487, { at: 4_010, to: "INVITE" })
      .ack({ at: 4_020, by: "party" })
    const { call } = callOf(s, a)

    expect(CallFacts.valueOf(call.gaveUp)).toMatchObject({ by: "CANCEL", entry: { at_us: 4_000_000, bySystem: false } })
  })

  it("gave up at its BYE, also where the system's own BYE crossed it", () => {
    const s = scenario()
    const a = caller(s)
      .response(200, { at: 900 })
      .ack({ at: 910, by: "party" })
      .bye({ at: 5_000, by: "system" })
      .bye({ at: 5_002, by: "party" })
      .response(200, { at: 5_010, by: "party" })
      .response(200, { at: 5_012, by: "system" })
    const { call } = callOf(s, a)

    expect(CallFacts.valueOf(call.gaveUp)).toMatchObject({ by: "BYE", entry: { at_us: 5_002_000, bySystem: false } })
  })
})

describe("an attempt's ring time", () => {
  it("runs from its INVITE and from its first 180 to the system's CANCEL", () => {
    const s = scenario()
    const a = caller(s)
    const b = cancelled(attempt(s, 100), 20_130)
    const { legs } = callOf(s, a, b)
    const ring = CallFacts.ringTime(legs[0]!)

    expect(CallFacts.valueOf(ring.fromInvite_us)).toBe(20_030_000)
    expect(CallFacts.valueOf(ring.fromRinging_us)).toBe(20_000_000)
    expect(CallFacts.valueOf(ring.end)).toMatchObject({ method: "CANCEL", at_us: 20_130_000 })
  })

  it("has no ringing part where no 18x came, or where the first one crossed the CANCEL", () => {
    const s = scenario()
    const a = caller(s)
    const silent = cancelled(attempt(s, 100, false), 5_100)
    const crossed = attempt(s, 6_000, false).cancel({ at: 9_000, by: "system" }).response(180, { at: 9_001, to: "INVITE" })
    const { legs } = callOf(s, a, silent, crossed)

    expect(CallFacts.ringTime(legs[0]!).fromRinging_us.state).toBe("absent")
    expect(CallFacts.valueOf(CallFacts.ringTime(legs[0]!).fromInvite_us)).toBe(5_000_000)
    expect(CallFacts.ringTime(legs[1]!).fromRinging_us.state).toBe("absent")
  })

  it("ends at the CANCEL where a failure final was stamped ahead of it", () => {
    const s = scenario()
    const a = caller(s)
    const b = attempt(s, 100).response(486, { at: 4_100 }).cancel({ at: 4_130, by: "system" })
    const { legs } = callOf(s, a, b)

    expect(CallFacts.valueOf(CallFacts.ringTime(legs[0]!).fromRinging_us)).toBe(4_000_000)
  })

  it("ends at the answer where the attempt was answered", () => {
    const s = scenario()
    const a = caller(s)
    const b = attempt(s, 100).response(200, { at: 3_130 }).ack({ at: 3_140, by: "system" })
    const { legs } = callOf(s, a, b)

    expect(CallFacts.valueOf(CallFacts.ringTime(legs[0]!).fromRinging_us)).toBe(3_000_000)
  })

  it("is not captured where the capture stops while the attempt rings", () => {
    const s = scenario()
    const a = caller(s)
    const b = attempt(s, 100)
    const { legs } = callOf(s, a, b)

    expect(CallFacts.ringTime(legs[0]!).fromInvite_us.state).toBe("not-captured")
  })
})

describe("the system released a dialled leg on its own", () => {
  const releaseOf = (s: Scenario, a: ScenarioLeg, b: ScenarioLeg): CallFacts.Release => {
    const { call, legs } = callOf(s, a, b)
    return CallFacts.release(call, legs[0]!, SKEW_US)
  }

  it("where it cancelled an attempt and the caller did nothing", () => {
    const s = scenario()
    const a = caller(s)
    const b = cancelled(attempt(s, 100), 20_130)
    a.response(480, { at: 20_200, to: "INVITE" }).ack({ at: 20_210, by: "party" })

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "own", ending: { by: "CANCEL", entry: { at_us: 20_130_000 } } })
  })

  it("but relayed the caller's CANCEL where that is stamped inside the skew after its own", () => {
    const s = scenario()
    const a = caller(s)
    const b = cancelled(attempt(s, 100), 20_130)
    a.cancel({ at: 20_430, by: "party" }).response(200, { at: 20_435 })

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "relayed", cause: "caller-gave-up", because: { at_us: 20_430_000 } })
  })

  it("on its own where the caller's CANCEL is stamped past the skew after its own", () => {
    const s = scenario()
    const a = caller(s)
    const b = cancelled(attempt(s, 100), 20_130)
    a.cancel({ at: 20_930, by: "party" }).response(200, { at: 20_935 })

    expect(releaseOf(s, a, b).kind).toBe("own")
  })

  it("but relayed a give-up the caller sent before the attempt was even dialled", () => {
    const s = scenario()
    const a = caller(s).cancel({ at: 90, by: "party" }).response(200, { at: 95 })
    const b = cancelled(attempt(s, 100), 2_000)

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "relayed", cause: "caller-gave-up" })
  })

  it("where the caller cancelled an earlier INVITE, sent a new one, and the attempt dialled for it timed out", () => {
    const s = scenario()
    const a = caller(s)
      .cancel({ at: 90, by: "party" })
      .response(200, { at: 95 })
      .response(487, { at: 100, to: "INVITE" })
      .ack({ at: 105, by: "party" })
      .invite({ at: 60_000, by: "party", fresh: true })
      .response(100, { at: 60_005 })
    const b = cancelled(attempt(s, 60_100), 80_000)
    a.response(480, { at: 80_200, to: "INVITE" }).ack({ at: 80_210, by: "party" })
    const { call, legs } = callOf(s, a, b)

    expect(call.gaveUp.state).toBe("absent")
    expect(CallFacts.release(call, legs[0]!, SKEW_US)).toMatchObject({ kind: "own", ending: { entry: { at_us: 80_000_000 } } })
  })

  /** A call with two caller legs: the first answered and hung up at 5 s, the second opened at 60 s. */
  const secondCaller = (s: Scenario): { readonly first: ScenarioLeg; readonly second: ScenarioLeg } => ({
    first: caller(s)
      .response(200, { at: 900 })
      .ack({ at: 910, by: "party" })
      .bye({ at: 5_000, by: "party" })
      .response(200, { at: 5_005 }),
    second: s.leg({ callId: "second-caller@example.com", party: CALLER }).invite({ at: 60_000, by: "party" })
  })

  const releaseOfTwo = (s: Scenario, callers: ReadonlyArray<ScenarioLeg>, b: ScenarioLeg): CallFacts.Release => {
    const flows = s.flows()
    const at = (leg: ScenarioLeg) => CallFacts.atVantage(flows, s.sut, leg.vantage())
    return CallFacts.release(CallFacts.ofCall({ caller: callers.map(at), chain: [[at(b)]] }), at(b), SKEW_US)
  }

  it("where another caller leg hung up long before the attempt was dialled", () => {
    const s = scenario()
    const { first, second } = secondCaller(s)
    const b = cancelled(attempt(s, 60_100), 80_000)
    second.response(480, { at: 80_200 }).ack({ at: 80_210, by: "party" })

    expect(releaseOfTwo(s, [first, second], b).kind).toBe("own")
  })

  it("but relayed the give-up of the caller leg that left while the attempt ran, whatever an earlier leg did", () => {
    const s = scenario()
    const { first, second } = secondCaller(s)
    const b = cancelled(attempt(s, 60_100), 80_000)
    second.cancel({ at: 79_990, by: "party" }).response(200, { at: 79_995 })

    expect(releaseOfTwo(s, [first, second], b)).toMatchObject({
      kind: "relayed",
      cause: "caller-gave-up",
      because: { at_us: 79_990_000 }
    })
  })

  it("where the caller's give-up is older than the attempt's INVITE by more than the skew", () => {
    const s = scenario()
    const a = caller(s).cancel({ at: 90, by: "party" }).response(200, { at: 95 })
    const b = cancelled(attempt(s, 700), 2_000)

    expect(releaseOf(s, a, b).kind).toBe("own")
  })

  it("but cancelled a fork's loser where the caller was answered while the attempt ran", () => {
    const s = scenario()
    const a = caller(s).response(200, { at: 8_000 }).ack({ at: 8_010, by: "party" })
    const b = cancelled(attempt(s, 100), 8_005)

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "relayed", cause: "caller-answered", because: { at_us: 8_000_000 } })
  })

  it("where it sent the BYE of an answered leg and the caller did nothing", () => {
    const s = scenario()
    const a = caller(s).response(200, { at: 900 }).ack({ at: 910, by: "party" })
    const b = attempt(s, 100)
      .response(200, { at: 800 })
      .ack({ at: 810, by: "system" })
      .bye({ at: 30_000, by: "system" })
      .response(200, { at: 30_010 })

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "own", ending: { by: "BYE" } })
  })

  it("but relayed the caller's BYE where the caller hung up first", () => {
    const s = scenario()
    const a = caller(s).response(200, { at: 900 }).ack({ at: 910, by: "party" }).bye({ at: 29_990, by: "party" }).response(200, { at: 29_995 })
    const b = attempt(s, 100)
      .response(200, { at: 800 })
      .ack({ at: 810, by: "system" })
      .bye({ at: 30_000, by: "system" })
      .response(200, { at: 30_010 })

    expect(releaseOf(s, a, b)).toMatchObject({ kind: "relayed", cause: "caller-gave-up" })
  })

  it("not at all where the leg's party ended it, or a failure final did", () => {
    const s = scenario()
    const a = caller(s).response(200, { at: 900 }).ack({ at: 910, by: "party" })
    const hungUp = attempt(s, 100).response(200, { at: 800 }).ack({ at: 810, by: "system" }).bye({ at: 30_000, by: "party" }).response(200, { at: 30_010 })
    const busy = attempt(s, 200, false).response(486, { at: 240 }).ack({ at: 250, by: "system" })
    const { call, legs } = callOf(s, a, hungUp, busy)

    expect(CallFacts.release(call, legs[0]!, SKEW_US).kind).toBe("none")
    expect(CallFacts.release(call, legs[1]!, SKEW_US).kind).toBe("none")
  })
})
