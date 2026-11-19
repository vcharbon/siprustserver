/**
 * Why one attempt of a chain handed over to the next, as read off the previous
 * attempt's own captured final.
 */
import { describe, expect, it } from "vitest"
import { sequentialChain as chain } from "../src/chain-reading.js"
import type * as Policy from "../src/policy.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import type * as Topology from "../src/topology.js"
import { actorOf } from "./scenario-actors.js"

const CALLER = "198.51.100.3:5060"
const CALLEE = "198.51.100.2:5060"

/** The caller's leg, opened `at` ms. */
const callerLeg = (s: Scenario, at: number): ScenarioLeg =>
  s.leg({ callId: "a0@example.com", party: CALLER }).invite({ at, by: "party" })

/** An attempt the platform dials `at` ms. */
const attemptLeg = (s: Scenario, id: string, at: number): ScenarioLeg =>
  s.leg({ callId: `${id}@example.com`, party: CALLEE }).invite({ at, by: "system" })

/** The chain reading of a caller leg (where the case holds one) and its attempts, in chain order on branch 0. */
const read = (s: Scenario, caller: ScenarioLeg | undefined, attempts: ReadonlyArray<ScenarioLeg>): Policy.ChainReading => {
  const flows = s.flows()
  const actorsObs = [
    ...(caller === undefined ? [] : [actorOf(s, flows, caller, { kind: "uac" })]),
    ...attempts.map((leg, position) => actorOf(s, flows, leg, { kind: "uas", pos: `called[0][${position}]` }))
  ]
  return chain(flows, { actorsObs, legs: [], branches: [] } as unknown as Topology.Layout)
}

/** A two-attempt chain whose first attempt ended on the callee's own final. */
const handover = (status: number): string | undefined => {
  const s = scenario()
  const first = attemptLeg(s, "b0", 1_000).response(status, { at: 1_100 })
  return read(s, undefined, [first, attemptLeg(s, "b1", 1_200)]).causes[0]?.cause
}

describe("the cause an arriving final states", () => {
  /**
   * A 408 the CALLEE answered with is an arriving status like any other: the
   * SUT posts it as an external failure, so a row keyed on the platform's own
   * Timer B would arm a reroute on a callback that never comes.
   */
  it("reads a callee 408 as an external status, not the platform's own timeout", () => {
    expect(handover(408)).toBe("external:408")
  })

  it("still reads the busy statuses as busy", () => {
    expect(handover(486)).toBe("busy")
    expect(handover(600)).toBe("busy")
  })

  it("carries any other rejection under its own status", () => {
    expect(handover(503)).toBe("external:503")
  })

  it("reads the final of the attempt's last INVITE where it was sent again after a failure", () => {
    const s = scenario()
    const first = attemptLeg(s, "b0", 1_000)
      .response(503, { at: 1_100 })
      .ack({ at: 1_110, by: "system" })
      .invite({ at: 1_500, by: "system", fresh: true })
      .response(486, { at: 1_600 })

    expect(read(s, undefined, [first, attemptLeg(s, "b1", 1_700)]).causes[0]?.cause).toBe("busy")
  })
})

/**
 * One attempt the SUT CANCELed 13 336 ms after its INVITE, while it rang from
 * 9 000 ms on (or on each of `provisionals`, `[ms after the INVITE, status]`),
 * with the caller's own leg beside it: what the caller sent `callerLead` ms
 * before that CANCEL decides whether the CANCEL is the platform's timer or a
 * propagation.
 */
const sutCancelAfter = (
  callerSent: "CANCEL" | "BYE" | undefined,
  provisionals: ReadonlyArray<readonly [number, number]> = [[9_000, 180]],
  callerLead = 1
): Policy.ChainReading => {
  const inviteAt = 1_000
  const cancelAt = inviteAt + 13_336
  const s = scenario()
  const caller = callerLeg(s, inviteAt - 22).response(180, { at: inviteAt + 9_000 })
  if (callerSent !== undefined) caller.request(callerSent, { at: cancelAt - callerLead, by: "party" })
  const attempt = attemptLeg(s, "b0", inviteAt)
  for (const [ms, status] of provisionals) attempt.response(status, { at: inviteAt + ms, to: "INVITE" })
  attempt.cancel({ at: cancelAt, by: "system" })
  return read(s, caller, [attempt])
}

describe("the cause a SUT-originated CANCEL states", () => {
  it("reads a CANCEL of a ringing attempt nothing else explains as the platform's no-answer timer", () => {
    const reading = sutCancelAfter(undefined)
    expect(reading.causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-answer", 4_336]])
    expect(reading.flags).toEqual([])
  })

  it("states the no-answer dwell from the attempt's first 18x, to the millisecond, not from its INVITE", () => {
    const reading = sutCancelAfter(undefined)
    expect(reading.causes[0]?.after_ms).toBe(13_336 - 9_000)
    expect(reading.causes[0]?.evidence[0]).toBe("SUT-originated CANCEL 4336 ms after the attempt's first 18x")
  })

  it("rounds the dwell to the nearest millisecond, as the ledger reads a span", () => {
    const reading = sutCancelAfter(undefined, [[9_000.4, 180]])
    expect(reading.causes.map((c) => c.after_ms)).toEqual([4_336])
  })

  it("counts a 183 as the first 18x, and a 180 after it moves nothing", () => {
    const reading = sutCancelAfter(undefined, [[2_500, 183], [9_000, 180]])
    expect(reading.causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-answer", 13_336 - 2_500]])
  })

  it("mints no timer when the caller CANCELed while the attempt ran", () => {
    const reading = sutCancelAfter("CANCEL")
    expect(reading.causes).toEqual([])
    expect(reading.flags).toEqual([])
  })

  /**
   * A caller hangs up an early dialog with a BYE (RFC 3261 §15.1): the SUT's
   * CANCEL that follows is a propagation, and a no-answer dwell minted from it
   * arms a timer the platform never had, which then CANCELs the callee before
   * the scripted BYE.
   */
  it("mints no timer when the caller BYEd the early dialog while the attempt ran", () => {
    const reading = sutCancelAfter("BYE")
    expect(reading.causes).toEqual([])
    expect(reading.flags).toEqual([])
  })

  /**
   * A caller that left before the platform dialled the attempt left the whole
   * call: the dial crossed its CANCEL, and the platform's CANCEL of that
   * attempt relays it.
   */
  it("mints no timer when the caller CANCELed before the attempt was dialled", () => {
    const reading = sutCancelAfter("CANCEL", [[9_000, 180]], 13_340)
    expect(reading.causes).toEqual([])
  })

  /**
   * The caller CANCELs its INVITE at 90 ms and sends a new one on the same
   * Call-ID a minute later (RFC 3261 §8.1.3.5): the attempt dialled for the
   * new INVITE rings for 19 870 ms and the SUT CANCELs it. The old CANCEL left
   * an INVITE the caller replaced; it is no hang-up of this call.
   */
  it("reads the no-answer timer where the caller cancelled an earlier INVITE and sent it again", () => {
    const s = scenario()
    const caller = callerLeg(s, 0)
      .cancel({ at: 90, by: "party" })
      .response(200, { at: 95 })
      .response(487, { at: 100, to: "INVITE" })
      .ack({ at: 105, by: "party" })
      .invite({ at: 60_000, by: "party", fresh: true })
    const attempt = attemptLeg(s, "b0", 60_100).response(180, { at: 60_130 }).cancel({ at: 80_000, by: "system" })

    expect(read(s, caller, [attempt]).causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-answer", 19_870]])
  })

  /**
   * The caller, answered before the attempt is dialled, re-offers and CANCELs
   * that re-INVITE (RFC 3261 §14.1) while the attempt rings: the CANCEL ends a
   * session change, not the call, and the SUT's CANCEL of the attempt is its
   * own timer.
   */
  it("reads the no-answer timer where the caller cancelled a re-INVITE while the attempt rang", () => {
    const s = scenario()
    const caller = callerLeg(s, 0)
      .response(200, { at: 400 })
      .ack({ at: 410, by: "party" })
      .invite({ at: 6_000, by: "party" })
      .cancel({ at: 6_200, by: "party" })
      .response(200, { at: 6_205 })
      .response(487, { at: 6_210, to: "INVITE" })
      .ack({ at: 6_215, by: "party" })
    const attempt = attemptLeg(s, "b0", 1_000).response(180, { at: 1_030 }).cancel({ at: 21_030, by: "system" })

    expect(read(s, caller, [attempt]).causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-answer", 20_000]])
  })

  /**
   * The SUT sends the attempt's INVITE again after a 503 (RFC 3261 §8.1.3.5)
   * and the callee rings on the new one: the timer the SUT's CANCEL states is
   * read on the INVITE that rang, from its 180.
   */
  it("reads the no-answer timer of an attempt sent again after a failure off its last INVITE's 18x", () => {
    const s = scenario()
    const attempt = attemptLeg(s, "b0", 1_000)
      .response(503, { at: 1_100 })
      .ack({ at: 1_110, by: "system" })
      .invite({ at: 1_500, by: "system", fresh: true })
      .response(180, { at: 2_000 })
      .cancel({ at: 21_000, by: "system" })

    expect(read(s, callerLeg(s, 0), [attempt]).causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-answer", 19_000]])
  })
})

/**
 * A two-attempt hunt whose first attempt the callee answered with 100 alone:
 * the SUT CANCELs it 10 000 ms after its INVITE, before any 18x, and hands
 * over to the second attempt. That CANCEL is the platform's no-provisional
 * timer, and its INVITE→CANCEL dwell is the
 * value that timer was armed with.
 */
const noProvisionalHandover = (): Policy.ChainReading => {
  const inviteAt = 1_000
  const cancelAt = inviteAt + 10_000
  const s = scenario()
  const caller = callerLeg(s, inviteAt - 40)
  const first = attemptLeg(s, "b0", inviteAt)
    .response(100, { at: inviteAt + 0.5 })
    .cancel({ at: cancelAt, by: "system" })
    .response(487, { at: cancelAt + 13, to: "INVITE" })
  return read(s, caller, [first, attemptLeg(s, "b1", cancelAt + 15)])
}

describe("the cause a SUT CANCEL of an attempt that sent no 18x states", () => {
  it("reads it as the no-provisional timer with its INVITE→CANCEL dwell, not an unreadable handover", () => {
    const reading = noProvisionalHandover()
    expect(reading.flags).toEqual([])
    expect(reading.causes.map((c) => [c.from, c.cause, c.after_ms])).toEqual([["called[0][0]", "no-provisional", 10_000]])
  })
})

describe("a platform CANCEL a vantage stamps just before the caller's own", () => {
  /**
   * The caller CANCELs at 5.000 s; the vantage stamps the platform's CANCEL to
   * the callee at 4.880 s, its relay captured on another interface 120 ms
   * early. No 18x came back. That CANCEL relays the caller's give-up: it is no
   * timer of the platform's, and arms no no-provisional timer. A propagated give-up
   * states no timer cause, and a last attempt hands over to nothing, so
   * nothing is flagged either.
   */
  it("is the caller's give-up, not a no-provisional timer", () => {
    const callerCancel = 5_000
    const s = scenario()
    const caller = callerLeg(s, 0).cancel({ at: callerCancel, by: "party" })
    const attempt = attemptLeg(s, "b0", 40).response(100, { at: 40.5 }).cancel({ at: callerCancel - 120, by: "system" })

    const reading = read(s, caller, [attempt])
    expect(reading.causes).toEqual([])
    expect(reading.flags).toEqual([])
  })

  it("is the platform's own timer where the caller's CANCEL is stamped past the relay skew after it", () => {
    const callerCancel = 5_000
    const s = scenario()
    const caller = callerLeg(s, 0).cancel({ at: callerCancel, by: "party" })
    const attempt = attemptLeg(s, "b0", 40).response(100, { at: 40.5 }).cancel({ at: callerCancel - 620, by: "system" })

    expect(read(s, caller, [attempt]).causes.map((c) => [c.cause, c.after_ms])).toEqual([["no-provisional", 4_340]])
  })
})

describe("a platform CANCEL of a fork's loser", () => {
  it("states no timer where the caller was answered while the attempt ran, and flags the handover it gates", () => {
    const s = scenario()
    const caller = callerLeg(s, 0).response(200, { at: 8_000 })
    const loser = attemptLeg(s, "b0", 40).response(180, { at: 70 }).cancel({ at: 8_005, by: "system" })

    const reading = read(s, caller, [loser, attemptLeg(s, "b1", 9_000)])
    expect(reading.causes).toEqual([])
    expect(reading.flags.map((f) => f.kind)).toEqual(["failover-cause-unreadable"])
    expect(reading.flags[0]?.detail).toContain("the caller was answered 5 ms before the SUT CANCEL")
  })
})

describe("the give-up timer of a failover leg, by where its CANCEL fell", () => {
  /**
   * A hunt: the first callee refuses 486 at once, the platform hunts to the
   * second, invited at 2 s, which the platform CANCELs at 22 s — after its
   * 180 at 5 s (`rang`), or with none.
   */
  const hunt = (rang: boolean) => {
    const second = 2_000
    const s = scenario()
    const caller = callerLeg(s, 0)
    const first = attemptLeg(s, "b0", 1_000).response(486, { at: 1_100 })
    const failover = attemptLeg(s, "b1", second)
    if (rang) failover.response(180, { at: second + 3_000 })
    failover.cancel({ at: second + 20_000, by: "system" })
    return read(s, caller, [first, failover]).causes.map((c) => [c.from, c.cause, c.after_ms])
  }

  it("is its ring from its first 18x when the CANCEL followed one", () => {
    expect(hunt(true)).toEqual([["called[0][0]", "busy", undefined], ["called[0][1]", "no-answer", 17_000]])
  })

  it("is its no-provisional timer from its INVITE when no 18x preceded the CANCEL", () => {
    expect(hunt(false)).toEqual([["called[0][0]", "busy", undefined], ["called[0][1]", "no-provisional", 20_000]])
  })
})
