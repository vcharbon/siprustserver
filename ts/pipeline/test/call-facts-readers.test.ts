/**
 * The cut's own readers of a capture, on the scenarios where a leg holds more
 * than its opening INVITE's first transaction: each reads the leg's outcome,
 * the final of its last dialog-opening INVITE, and never a re-INVITE's.
 */
import { describe, expect, it } from "vitest"
import { incompleteSourceCall } from "../src/call-completeness.js"
import { buildCalls } from "../src/calls.js"
import { neutralPolicy } from "../src/policy.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import { build } from "../src/topology.js"
import { plan } from "./fixtures.js"

const CALLER = "198.51.100.10:5060"
const FIRST = "203.0.113.20:5060"
const SECOND = "203.0.113.21:5060"

const callerLeg = (s: Scenario): ScenarioLeg =>
  s
    .leg({ callId: "caller@example.com", party: CALLER, from: "sip:+15556000001@example.com", to: "sip:+15556000004@example.com" })
    .invite({ at: 0, by: "party" })
    .response(100, { at: 5 })

const layoutOf = (s: Scenario, legs: ReadonlyArray<ScenarioLeg>) => {
  const flows = s.flows()
  return { flows, layout: build(flows, legs.map((leg) => leg.vantage()), s.sut, plan(), () => false) }
}

describe("an attempt whose INVITE was sent again after a failure", () => {
  const retried = (s: Scenario): ScenarioLeg =>
    s
      .leg({ callId: "first@example.com", party: FIRST, to: "sip:+15556000004@example.com" })
      .invite({ at: 40, by: "system" })
      .response(503, { at: 60 })
      .ack({ at: 65, by: "system" })
      .invite({ at: 500, by: "system", fresh: true })
      .response(180, { at: 530 })

  it("states the retry's final as the attempt's final", () => {
    const s = scenario()
    const caller = callerLeg(s)
    const first = retried(s).response(486, { at: 9_000, to: "INVITE" }).ack({ at: 9_010, by: "system" })
    caller.response(486, { at: 9_020, to: "INVITE" }).ack({ at: 9_030, by: "party" })
    const { flows, layout } = layoutOf(s.group(caller, first), [caller, first])

    const { calls } = buildCalls(flows, layout, 0, "A", neutralPolicy, [])

    expect(calls[0]!.attempts.map((a) => a.final)).toEqual([{ status: 486, at_ms: 9_000 }])
  })

  it("is still running, so a leg dialled before its last INVITE was given up is a second branch", () => {
    const s = scenario()
    const caller = callerLeg(s)
    const first = retried(s)
    const second = s
      .leg({ callId: "second@example.com", party: SECOND, to: "sip:+15556000005@example.com" })
      .invite({ at: 15_000, by: "system" })
      .response(486, { at: 15_040 })
      .ack({ at: 15_050, by: "system" })
    first.cancel({ at: 20_000, by: "system" }).response(200, { at: 20_005 }).response(487, { at: 20_020, to: "INVITE" }).ack({ at: 20_030, by: "system" })
    caller.response(486, { at: 20_040, to: "INVITE" }).ack({ at: 20_050, by: "party" })
    const { layout } = layoutOf(s.group(caller, first, second), [caller, first, second])

    const posOf = (leg: ScenarioLeg) => layout.actorsObs.find((a) => a.origLeg === leg.index)?.pos
    expect([posOf(first), posOf(second)]).toEqual(["called[0][0]", "called[1][0]"])
  })

  it("hands over to a leg dialled after the system cancelled its last INVITE", () => {
    const s = scenario()
    const caller = callerLeg(s)
    const first = retried(s)
      .cancel({ at: 20_000, by: "system" })
      .response(200, { at: 20_005 })
      .response(487, { at: 20_020, to: "INVITE" })
      .ack({ at: 20_030, by: "system" })
    const second = s
      .leg({ callId: "second@example.com", party: SECOND, to: "sip:+15556000005@example.com" })
      .invite({ at: 20_010, by: "system" })
      .response(486, { at: 20_060 })
      .ack({ at: 20_070, by: "system" })
    caller.response(486, { at: 20_080, to: "INVITE" }).ack({ at: 20_090, by: "party" })
    const { layout } = layoutOf(s.group(caller, first, second), [caller, first, second])

    const posOf = (leg: ScenarioLeg) => layout.actorsObs.find((a) => a.origLeg === leg.index)?.pos
    expect([posOf(first), posOf(second)]).toEqual(["called[0][0]", "called[0][1]"])
  })
})

describe("the caller's dialog, where a re-INVITE's 2xx is the last one captured", () => {
  const refreshed = (s: Scenario): ScenarioLeg =>
    callerLeg(s)
      .response(200, { at: 40 })
      .ack({ at: 50, by: "party" })
      .invite({ at: 600, by: "party" })

  const finding = (s: Scenario, leg: ScenarioLeg) =>
    incompleteSourceCall(s.flows(), s.sut, "capture", "case", [leg.vantage()])

  it("is complete where the BYE was captured ahead of that 2xx (RFC 3261 §15)", () => {
    const s = scenario()
    const leg = refreshed(s)
      .bye({ at: 610, by: "system" })
      .response(200, { at: 620, to: "INVITE" })
      .response(200, { at: 630, to: "BYE" })

    expect(finding(s, leg)).toBeUndefined()
  })

  it("is incomplete with no BYE, and the evidence names the 2xx that answered the call", () => {
    const s = scenario()
    const leg = refreshed(s).response(200, { at: 620 }).ack({ at: 630, by: "party" })

    expect(finding(s, leg)?.evidence).toMatchObject([{ leg: 0, hop: 0, msg: 2 }])
  })
})
