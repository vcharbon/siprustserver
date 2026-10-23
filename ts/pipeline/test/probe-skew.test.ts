/**
 * The merged-capture refusal: what a datagram written twice, once per probe,
 * costs the document — and what it does NOT cost.
 *
 * Driven through the rule rather than the predicate, because
 * the token and the finding line are the contract a refusal record carries.
 */
import { SutSet } from "../src/sut.js"
import { describe, expect, it } from "vitest"
import { MERGED_PROBE_SKEW, PROBE_SKEW_RULE } from "../src/probe-skew.js"
import { CAPTURE_RULES } from "../src/capture-rules.js"
import { doc, leg, msg, SUT_INGRESS, SUT_IP, UPSTREAM } from "./cutfixtures.js"

const CALL = "probe-skew@host"
const CAPTURE = "merged.pcap.gz"

/** One INVITE/100/180/CANCEL exchange, each message optionally doubled. */
const call = (doubles: ReadonlyArray<{ readonly at: number; readonly probe: number }>) => {
  const wire = [
    { seq: 1, method: "INVITE", src: UPSTREAM, dst: SUT_INGRESS, at_ms: 0 },
    { seq: 1, method: "INVITE", status: 100, src: SUT_INGRESS, dst: UPSTREAM, at_ms: 20 },
    { seq: 1, method: "INVITE", status: 180, src: SUT_INGRESS, dst: UPSTREAM, at_ms: 200 },
    { seq: 1, method: "CANCEL", src: UPSTREAM, dst: SUT_INGRESS, at_ms: 10_000 }
  ]
  const msgs = wire.map((w) =>
    msg({ callId: CALL, seq: w.seq, method: w.method, status: w.status, src: w.src, dst: w.dst, hop: 0, ts_ms: w.at_ms })
  )
  // Each double is the SAME message written again by another probe, later by
  // that probe's clock offset.
  for (const d of doubles) {
    const original = wire[d.at]!
    msgs.push(
      msg({
        callId: CALL,
        seq: original.seq,
        method: original.method,
        status: original.status,
        src: original.src,
        dst: original.dst,
        hop: 0,
        ts_ms: original.at_ms + 7900,
        probe: d.probe
      })
    )
  }
  return doc([leg(CALL, [{ a: UPSTREAM, b: SUT_INGRESS }], msgs)], [{ legs: [0] }])
}

const refusalOf = (flows: ReturnType<typeof call>) =>
  [PROBE_SKEW_RULE]
    .filter((rule) => rule.id === MERGED_PROBE_SKEW)
    .flatMap((rule) => {
      const finding = rule.refuses({
        flows,
        capture: CAPTURE,
        sut: new SutSet([SUT_IP]),
        legs: [0],
        callIds: [CALL],
        caseId: "case1"
      })
      return finding === undefined ? [] : [finding]
    })[0]

describe("a capture that merges probes whose clocks disagree", () => {
  it("refuses the call whose datagrams the merge wrote twice", () => {
    const finding = refusalOf(
      call([
        { at: 0, probe: 1 },
        { at: 1, probe: 1 },
        { at: 2, probe: 1 }
      ])
    )
    expect(finding).toBeDefined()
    expect(finding!.legs).toEqual([0])
    expect(finding!.evidence).toHaveLength(3)
    // The line says what was seen, why it is not a retransmission, and what
    // leaving it in would do to the document.
    expect(finding!.line).toContain("merges probes whose clocks disagree")
    expect(finding!.line).toContain("written TWICE, once per probe")
    expect(finding!.line).toContain("not")
    expect(finding!.line).toContain("retransmissions")
    expect(finding!.line).toContain("7900.0 ms")
  })

  /** THE DISCRIMINATOR. Identical bytes on ONE probe is the wire repeating
   *  itself, which is a fact about the call and must reach the cut. */
  it("leaves a same-probe repeat alone, however far apart", () => {
    expect(
      refusalOf(
        call([
          { at: 0, probe: 0 },
          { at: 1, probe: 0 },
          { at: 2, probe: 0 }
        ])
      )
    ).toBeUndefined()
  })

  /** A double is usually harmless — the cut declares a repeat, the SUT rides a
   *  real ladder, the counts agree. Only a call doubled past the threshold has
   *  no trustworthy order left. */
  it("leaves a call with too few doubled datagrams alone", () => {
    expect(refusalOf(call([{ at: 0, probe: 1 }, { at: 1, probe: 1 }]))).toBeUndefined()
  })

  it("names its own token", () => {
    expect(CAPTURE_RULES.map((r) => r.id)).toContain(MERGED_PROBE_SKEW)
  })
})
