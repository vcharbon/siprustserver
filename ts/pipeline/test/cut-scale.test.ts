/**
 * The cut over a capture holding THOUSANDS of concurrent dialogs: the families
 * it finds are the pairwise reference's, and the work it spends finding them is
 * linear in the legs, not in their pairs — and spent once per capture, not
 * once per case.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { decideCases } from "../src/caseset.js"
import { callFamilies, cutCalls } from "../src/cut.js"
import type { CallIdDerivation } from "../src/derivation.js"
import { policyWith } from "../src/policy.js"
import { doc, leg, oneHop, plan, request, response, SOCKETS, sutSet } from "./fixtures.js"

/** The shortest base a suffix match is evidence rather than coincidence. */
const MIN_BASE = 8

/** The decoys of {@link manyLegsFlows}: a suffix of every Call-ID, each under the base minimum. */
const DECOYS = ["example", "xample", "e"] as const

/** The longest derivation chain {@link manyLegsFlows} mints: `2-1-<base>` off `1-<base>` off `<base>`. */
const MAX_DEPTH = 2

/**
 * The candidate bases one leg can have in {@link manyLegsFlows}: every decoy
 * plus one per link of the chain below it — the most the predicate is asked
 * about it.
 */
const CANDIDATES_PER_LEG = DECOYS.length + MAX_DEPTH

/** A prefix-minting convention: the derived Call-ID ENDS WITH its base. */
const suffixDerives: CallIdDerivation = (base, derived) =>
  base.length >= MIN_BASE && derived.length > base.length && derived.endsWith(base)

/** The same predicate, counting how often the cut asks it. */
const counting = (inner: CallIdDerivation) => {
  let n = 0
  const derives: CallIdDerivation = (base, derived) => {
    n += 1
    return inner(base, derived)
  }
  return { derives, count: () => n }
}

/** A deterministic shuffle, so related legs are not adjacent in the document. */
const shuffled = <T>(items: ReadonlyArray<T>): Array<T> => {
  const out = [...items]
  let seed = 0x2545f491
  for (let i = out.length - 1; i > 0; i--) {
    seed = (Math.imul(seed, 1103515245) + 12345) >>> 0
    const j = seed % (i + 1)
    ;[out[i], out[j]] = [out[j]!, out[i]!]
  }
  return out
}

/**
 * Thousands of legs, message-free: base Call-IDs, `1-` and `2-1-` derivations
 * of them, bases nothing derives from, derivations whose base is absent, one
 * Call-ID two legs share, and the {@link DECOYS}.
 */
const manyLegsFlows = (n: number): Flows.FlowsDoc => {
  const ids: Array<string> = []
  for (let i = 0; i < n; i++) {
    const base = `call-${i.toString(16).padStart(6, "0")}@host.example`
    if (i % 11 === 0) {
      ids.push(`1-orphan-${i}@host.example`)
      continue
    }
    ids.push(base)
    if (i % 5 !== 0) ids.push(`1-${base}`)
    if (i % 7 === 0) ids.push(`2-1-${base}`)
    if (i % 13 === 0) ids.push(base)
  }
  ids.push(...DECOYS)
  return doc(
    shuffled(ids).map((id) => leg(id, oneHop(SOCKETS.caller, SOCKETS.sut), [])),
    []
  )
}

/** The all-pairs transitive closure, the reference the index must equal. */
const pairwiseFamilies = (
  flows: Flows.FlowsDoc,
  derives: CallIdDerivation
): ReadonlyArray<ReadonlyArray<number>> => {
  const parent = flows.legs.map((_, i) => i)
  const find = (i: number): number => {
    let r = i
    while (parent[r] !== r) r = parent[r]!
    return r
  }
  for (let i = 0; i < flows.legs.length; i++) {
    for (let j = i + 1; j < flows.legs.length; j++) {
      const ci = flows.legs[i]!.call_id
      const cj = flows.legs[j]!.call_id
      if (derives(ci, cj) || derives(cj, ci)) {
        const ri = find(i)
        const rj = find(j)
        if (ri !== rj) parent[Math.max(ri, rj)] = Math.min(ri, rj)
      }
    }
  }
  const by = new Map<number, Array<number>>()
  flows.legs.forEach((_, i) => {
    const r = find(i)
    by.set(r, [...(by.get(r) ?? []), i])
  })
  return [...by.values()].sort((a, b) => a[0]! - b[0]!)
}

/** `n` answered calls through the SUT, each a caller leg and a derived callee leg. */
const manyCallsFlows = (n: number): Flows.FlowsDoc => {
  const legs: Array<Flows.Leg> = []
  for (let i = 0; i < n; i++) {
    const a = `caller-${i.toString(16).padStart(6, "0")}@10.0.0.9`
    const b = `1-${a}`
    const t = i * 20_000
    const { callee, caller, sut } = SOCKETS
    legs.push(
      leg(a, oneHop(caller, sut), [
        request({ callId: a, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: t }),
        response({ callId: a, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: t + 5 }),
        response({ callId: a, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: t + 200, toTag: "sut-tag" }),
        response({ callId: a, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: t + 1_000, toTag: "sut-tag" }),
        request({ callId: a, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: t + 1_005, toTag: "sut-tag" }),
        request({ callId: a, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: t + 5_000, toTag: "sut-tag" }),
        response({ callId: a, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: t + 5_005, toTag: "sut-tag" })
      ]),
      leg(b, oneHop(sut, callee), [
        request({ callId: b, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: t + 10 }),
        response({ callId: b, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: t + 15 }),
        response({ callId: b, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: t + 190, toTag: "callee-tag" }),
        response({ callId: b, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: t + 990, toTag: "callee-tag" }),
        request({ callId: b, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: t + 1_010, toTag: "callee-tag" }),
        request({ callId: b, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: t + 5_010, toTag: "callee-tag" }),
        response({ callId: b, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: t + 5_015, toTag: "callee-tag" })
      ])
    )
  }
  return doc(shuffled(legs), [])
}

describe("call families over thousands of legs", () => {
  const flows = manyLegsFlows(1_500)

  it("are the pairwise closure's, decoys and orphans included", () => {
    expect(callFamilies(flows, suffixDerives)).toEqual(pairwiseFamilies(flows, suffixDerives))
  })

  it("ask the derivation about candidate bases only, linearly in the legs", () => {
    const counted = counting(suffixDerives)
    callFamilies(flows, counted.derives)
    // A leg's candidate bases are its own suffixes that are another leg's
    // Call-ID, and the predicate is asked about those alone, never about every
    // other leg.
    expect(counted.count()).toBeLessThanOrEqual(CANDIDATES_PER_LEG * flows.legs.length)
  })
})

describe("deciding every case of a capture", () => {
  it("correlates the capture once, not once per case", () => {
    const flows = manyCallsFlows(150)
    const sut = sutSet()
    const cut = cutCalls(flows, sut, "many.pcap", suffixDerives)
    expect(cut.calls).toHaveLength(150)
    const specs = cut.calls.map((c) => ({
      id: c.id,
      uac: c.uac,
      uas: [...c.uas],
      defects: [],
      cutLegs: c.legs
    }))

    const counted = counting(suffixDerives)
    const decided = decideCases({
      flows,
      capture: "many.pcap",
      specs,
      sut,
      plan: plan(),
      policy: policyWith({ derives: counted.derives })
    })
    expect(decided.outcomes.every((o) => o.built !== undefined && o.error === undefined)).toBe(true)
    expect(decided.refused).toEqual([])
    // One probe per derived leg, its base being its only suffix that is a
    // Call-ID of the document; a base leg has no suffix as long as the shortest.
    expect(counted.count()).toBe(flows.legs.length / 2)
  })
})
