import { describe, expect, it } from "vitest"
import { classify, type StepTiming } from "../src/delay.js"

const step = (
  leg: string,
  emits: boolean,
  ts_ms: number,
  typeKey: string,
  timerLinked = false,
  cseq = 1
): StepTiming => ({ leg, emits, ts_us: ts_ms * 1000, typeKey, cseq, timerLinked })

describe("relay dwells (§6.8)", () => {
  /**
   * A caller INVITE and its relay, 66 ms apart on the wire. The message offers
   * a session timer, so the step carries the timer flag in.
   */
  const relay = (): ReadonlyArray<StepTiming> => [
    step("A", true, 0, "req:INVITE", true),
    step("B", false, 66, "req:INVITE", true)
  ]

  it("declares the synthetic 0 anchored at the send that caused it", () => {
    const [, relayed] = classify(relay())
    expect(relayed).toMatchObject({ derived: "propagated", ms: 0, from: "step:1" })
  })

  it("never gates a relay on a timer, whatever the message offers", () => {
    const [, relayed] = classify(relay())
    // The 0 is synthetic and the hop is ours, so there is nothing to measure it
    // against: timer_linked here would ask a lane to relay in exactly 0 ms.
    expect(relayed.timer_linked).toBe(false)
  })

  it("keeps the flag on a dwell the capture actually measured", () => {
    const [, refresh] = classify([
      step("A", true, 0, "req:INVITE", true),
      step("A", true, 1_800_000, "req:INVITE", true)
    ])
    expect(refresh).toMatchObject({ derived: "measured", ms: 1_800_000, timer_linked: true })
  })
})

describe("coincident originations (§6.8)", () => {
  /**
   * Two in-dialog polls originating on opposite legs 2 ms apart, each ~2039 s
   * after its own leg's ACK — and those two ACKs are 43 ms apart because the
   * captured platform took that long to relay one.
   */
  const polls = (): ReadonlyArray<StepTiming> => [
    step("A", true, 0, "req:INVITE"),
    step("B", false, 136, "req:INVITE"),
    step("B", true, 7541, "resp:200:INVITE"),
    step("A", false, 7555, "resp:200:INVITE"),
    step("A", true, 7578, "req:ACK"),
    step("B", false, 7622, "req:ACK"),
    step("A", true, 2_047_171, "req:OPTIONS"),
    step("B", true, 2_047_173, "req:OPTIONS")
  ]

  it("anchors the later poll on the earlier, not on its own leg's ACK", () => {
    const d = classify(polls())
    // Anchored on step 5 (leg A's ACK) and step 3 the two dwells would be
    // 2039592 and 2039550 — a 2 ms order expressed as the difference of two
    // 2039 s dwells hung off a relay latency, which no lane can hold.
    expect(d[6]).toMatchObject({ derived: "measured", from: "step:5", ms: 2_039_593 })
    expect(d[7]).toMatchObject({ derived: "measured", from: "step:7", ms: 2 })
  })

  it("leaves a send whose own leg moved more recently alone", () => {
    // Leg B's 200 is 7541 ms in and leg A last emitted at 0, behind leg B's own
    // arrival at 136: the cross-leg emit is not the nearer instant, so the
    // same-leg anchor stands.
    expect(classify(polls())[2]).toMatchObject({ from: "step:2", ms: 7405 })
  })

  it("never borrows an arrival as the anchor", () => {
    // Leg B's only earlier step is the expect at 136 ms; an arrival's instant is
    // the SUT's to choose, so leg B's poll falls back to its own leg.
    const d = classify([
      step("A", true, 0, "req:INVITE"),
      step("B", false, 136, "req:INVITE"),
      step("B", true, 1_000, "req:OPTIONS")
    ])
    expect(d[2]).toMatchObject({ from: "step:2", ms: 864 })
  })

  it("keeps a distant cross-leg emit out of it", () => {
    const d = classify([
      step("A", true, 0, "req:OPTIONS"),
      step("B", true, 3_000, "req:OPTIONS")
    ])
    // Beyond the window the two are not one origination pair, so leg B's poll
    // is an ordinary measured dwell rather than the order-preserving anchor —
    // and the dwell is the 3 s the capture holds, not a zero.
    expect(d[1]).toMatchObject({ derived: "measured", from: "step:1", ms: 3_000 })
  })

  it("leaves a send an earlier arrival is already waiting for on its own leg", () => {
    // The capture shows leg A's 180 BEFORE leg B sent one, so the arrival is
    // listed ahead of its own origin and the classifier reads it
    // sut-originated. Re-anchoring the 180 onto leg A's PRACK would deadlock:
    // the PRACK waits on the arrival, the arrival on the 180.
    const d = classify([
      step("A", true, 0, "req:INVITE"),
      step("A", false, 17, "resp:100:INVITE"),
      step("B", false, 82, "req:INVITE"),
      step("B", true, 118, "resp:100:INVITE"),
      step("A", false, 215, "resp:180:INVITE"),
      step("A", true, 226, "req:PRACK"),
      step("B", true, 231, "resp:180:INVITE")
    ])
    expect(d[6]).toMatchObject({ derived: "measured", from: "step:4", ms: 113 })
  })

  it("still re-anchors a later poll pair once an earlier one has its origin", () => {
    // A call that polls twice. The first pair's arrivals are relayed and have
    // origins, so they are nobody's hostage — the second pair must not be
    // refused on their account.
    const d = classify([
      step("A", true, 0, "req:INVITE"),
      step("B", false, 33, "req:INVITE"),
      step("A", true, 1_020_252, "req:OPTIONS"),
      step("B", true, 1_020_253, "req:OPTIONS"),
      step("B", false, 1_020_254, "req:OPTIONS"),
      step("A", false, 1_020_256, "req:OPTIONS"),
      step("A", true, 5_100_263, "req:OPTIONS"),
      step("B", true, 5_100_265, "req:OPTIONS")
    ])
    expect(d[3]).toMatchObject({ from: "step:3", ms: 1 })
    expect(d[7]).toMatchObject({ from: "step:7", ms: 2 })
  })
})

describe("a leg's head step (§6.8)", () => {
  /**
   * A serial reroute: the callee rings for forty seconds, answers 480, and the
   * SUT opens a second attempt on leg C a fraction of a millisecond later. Leg
   * C's INVITE is forty seconds past the caller's, so `relayOriginOf` finds it
   * no origin and the head-of-leg anchor is what the document gets.
   */
  const reroute = (): ReadonlyArray<StepTiming> => [
    step("A", true, 0, "req:INVITE"),
    step("B", false, 98, "req:INVITE"),
    step("B", true, 40_185, "resp:480:INVITE"),
    step("B", false, 40_186, "req:ACK"),
    step("C", false, 40_186, "req:INVITE")
  ]

  it("anchors the second attempt on the failed attempt's last step", () => {
    // `trigger + 0` would open s5's budget at case start and expire it 3 s
    // before the 480 that provokes the reroute is even due.
    expect(classify(reroute())[4]).toMatchObject({
      derived: "sut-originated",
      from: "step:4",
      ms: 0
    })
  })

  it("keeps the gap the capture measured, however wide", () => {
    const d = classify([
      step("A", true, 0, "req:INVITE"),
      step("B", false, 66, "req:INVITE"),
      step("C", false, 40_186, "req:INVITE")
    ])
    expect(d[2]).toMatchObject({ from: "step:2", ms: 40_120 })
  })

  it("never lets a session timer gate a head dwell", () => {
    // The second attempt's INVITE offers a session timer, but the dwell that
    // reaches it crosses from leg B to leg C and no system timer runs across
    // two legs: measured, the 7 ms is the captured platform's own reroute
    // latency and §9.2 would hold a fresh SUT to it.
    const d = classify([
      step("A", true, 0, "req:INVITE", true),
      step("B", false, 66, "req:INVITE", true),
      step("B", true, 40_185, "resp:480:INVITE"),
      step("B", false, 40_186, "req:ACK"),
      step("C", false, 40_193, "req:INVITE", true)
    ])
    expect(d[4]).toMatchObject({ from: "step:4", ms: 7, timer_linked: false })
  })

  it("leaves the case's own first step on the trigger", () => {
    expect(classify(reroute())[0]).toMatchObject({ from: "trigger", ms: 0 })
  })

  it("never anchors a head send on a step already waiting for it", () => {
    // Leg B's 180 opens its leg and leg A's 180 is listed ahead of it with no
    // origin: anchoring the send on that arrival would leave each waiting on
    // the other, so the send keeps the run's start.
    const d = classify([
      step("A", true, 0, "req:INVITE"),
      step("A", false, 215, "resp:180:INVITE"),
      step("B", true, 231, "resp:180:INVITE")
    ])
    expect(d[2]).toMatchObject({ from: "trigger", ms: 0 })
  })
})

describe("a minted arrival is measured inside its own transaction (§6.8)", () => {
  /**
   * `capture_238065`'s BYE glare on leg B: the callee's own BYE (CSeq 1) and
   * the SUT's relay of the caller's BYE (CSeq 2) cross, so the callee's 481 to
   * one transaction lands between the other's request and its 200.
   */
  const glare = (): ReadonlyArray<StepTiming> => [
    step("B", false, 13_315, "req:BYE", false, 2),
    step("B", true, 13_320, "req:BYE", false, 1),
    step("B", true, 13_323, "resp:481:BYE", false, 2),
    step("B", false, 13_325, "resp:200:BYE", false, 1)
  ]

  it("anchors the 200 on the BYE it answers, not on the 481 that interleaved", () => {
    const d = classify(glare())
    expect(d[3]).toMatchObject({ derived: "sut-originated", from: "step:2", ms: 5 })
  })

  it("leaves the 481 on its leg's previous step, as every send is", () => {
    // A send's dwell is the actor's own decision and it decides from whatever
    // last happened on its leg, transaction or not.
    expect(classify(glare())[2]).toMatchObject({ derived: "measured", from: "step:2", ms: 3 })
  })

  it("falls back to the leg where the transaction has no earlier step", () => {
    // A poll the SUT originates opens its own transaction, so there is nothing
    // inside it to measure from.
    const d = classify([
      step("A", true, 0, "req:INVITE", false, 1),
      step("A", false, 17, "resp:100:INVITE", false, 1),
      step("A", false, 5_000, "req:OPTIONS", false, 20)
    ])
    expect(d[2]).toMatchObject({ derived: "sut-originated", from: "step:2", ms: 4_983 })
  })
})
