import { describe, expect, it } from "vitest"
import type { DelayCausality } from "../src/delay.js"
import type { StepDraft } from "../src/draft.js"
import { stampOverlaps } from "../src/race.js"

const D = { ms: 0, from: "trigger", compressible: true, timer_linked: false }

const step = (
  id: string,
  leg: string,
  op: "send" | "expect",
  from: string,
  ms = 0
): StepDraft =>
  ({
    id,
    leg,
    op,
    msg: { method: "BYE" },
    delay: { ...D, from, ms }
  }) as StepDraft

/**
 * The BYE glare of `capture_165814`: the callee's BYE (s11, leg B) is relayed
 * onto leg A while the caller's own BYE is 22 ms into its dwell off the same
 * anchor. Which lands first is (22 ms) against the SUT's relay latency.
 */
const glare = (): Array<StepDraft> => [
  step("s11", "B", "send", "step:s10", 7380),
  step("s12", "B", "expect", "step:s11", 11),
  step("s13", "A", "send", "step:s11", 22),
  step("s14", "A", "expect", "step:s11", 0),
  step("s15", "A", "expect", "step:s14", 1)
]

const causality = (...d: Array<DelayCausality>): Array<DelayCausality> => d

describe("declared races (§6.1)", () => {
  it("stamps the neighbour pair a shared anchor leaves unordered", () => {
    const steps = glare()
    stampOverlaps(
      steps,
      causality("measured", "propagated", "measured", "propagated", "propagated")
    )
    expect(steps[3]!.overlap).toBe("s13")
  })

  it("leaves every other step alone", () => {
    const steps = glare()
    stampOverlaps(
      steps,
      causality("measured", "propagated", "measured", "propagated", "propagated")
    )
    expect(steps.filter((s) => s.overlap !== undefined).map((s) => s.id)).toEqual(["s14"])
  })

  it("does not race two steps the document measured from different anchors", () => {
    const steps = glare()
    steps[2] = step("s13", "A", "send", "step:s9", 22)
    stampOverlaps(
      steps,
      causality("measured", "propagated", "measured", "propagated", "propagated")
    )
    expect(steps[3]!.overlap).toBeUndefined()
  })

  it("races a send with an expect the SUT minted itself, on the same anchor", () => {
    // A minted arrival is anchored inside its own transaction, so it shares an
    // anchor with a neighbouring send exactly when that send interleaved
    // between the two — an instant the SUT timed against a dwell the document
    // holds, which is the race.
    const steps = glare()
    stampOverlaps(
      steps,
      causality("measured", "propagated", "measured", "sut-originated", "propagated")
    )
    expect(steps[3]!.overlap).toBe("s13")
  })

  it("does not race a send with the expect that follows it on its own chain", () => {
    // s15 is measured from s14, not from what s14 is measured from: the leg
    // states an order of its own and nothing crossed it.
    const steps = glare()
    stampOverlaps(
      steps,
      causality("measured", "propagated", "measured", "propagated", "sut-originated")
    )
    expect(steps[4]!.overlap).toBeUndefined()
  })

  it("does not race two relays, neither of which the document paces", () => {
    const steps = glare()
    steps[2] = step("s13", "A", "expect", "step:s11", 22)
    stampOverlaps(
      steps,
      causality("measured", "propagated", "propagated", "propagated", "propagated")
    )
    expect(steps[3]!.overlap).toBeUndefined()
  })
})

/**
 * The teardown frontier of `capture_199978` case2 (leg A): a NOTIFY still
 * pending when the caller's BYE lands, and the two finals 1.5 ms apart against
 * a capture that never moved a message in under 6.7 ms. `s19` is the leg-B
 * relay of the NOTIFY, and the only thing that states the floor.
 */
const teardown = (): Array<StepDraft> => [
  seen(step("s17", "A", "send", "step:s16", 2848), 3_324_990),
  seen(step("s18", "A", "send", "step:s17", 6), 3_330_991),
  seen(step("s19", "B", "expect", "step:s17", 0), 3_331_668),
  seen(step("s20", "A", "expect", "step:s17", 14), 3_339_294),
  seen(step("s21", "A", "expect", "step:s18", 9), 3_340_818)
]

const seen = (s: StepDraft, at_us: number): StepDraft =>
  ({ ...s, observed: { leg: 0, msg: 0, at_us } }) as StepDraft

const frontier = causality("measured", "measured", "propagated", "propagated", "sut-originated")

describe("declared races: two arrivals answering two of our own sends", () => {
  it("races the two finals a teardown frontier leaves unordered", () => {
    const steps = teardown()
    stampOverlaps(steps, frontier)
    expect(steps[4]!.overlap).toBe("s20")
  })

  it("still races the send/arrival pair sharing the NOTIFY's anchor", () => {
    const steps = teardown()
    stampOverlaps(steps, frontier)
    expect(steps[3]!.overlap).toBe("s18")
  })

  it("holds the order where the gap reaches the capture's own relay floor", () => {
    const steps = teardown()
    steps[4] = seen(step("s21", "A", "expect", "step:s18", 9), 3_346_000)
    stampOverlaps(steps, frontier)
    expect(steps[4]!.overlap).toBeUndefined()
  })

  it("withholds the stamp where the capture demonstrates no relay at all", () => {
    const steps = teardown()
    stampOverlaps(
      steps,
      causality("measured", "measured", "sut-originated", "sut-originated", "sut-originated")
    )
    expect(steps[4]!.overlap).toBeUndefined()
  })

  it("does not race two arrivals whose anchor is not a send on their leg", () => {
    const steps = teardown()
    steps[4] = seen(step("s21", "A", "expect", "step:s20", 9), 3_340_818)
    stampOverlaps(steps, frontier)
    expect(steps[4]!.overlap).toBeUndefined()
  })
})
