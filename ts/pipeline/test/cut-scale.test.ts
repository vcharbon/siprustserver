/**
 * The cut over a capture holding THOUSANDS of concurrent dialogs: the families
 * it finds are the pairwise reference's, and the work it spends finding them is
 * linear in the legs, not in their pairs — and spent once per capture, not
 * once per case.
 */
import type { Flows } from "@sip/contracts"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import { captureIndex, type CaptureIndex } from "../src/capture-index.js"
import { CaseAssembler } from "../src/case-assembler.js"
import { callFamilies, cutCalls, type CutResult } from "../src/cut.js"
import type { CallIdDerivation } from "../src/derivation.js"
import { LogicExtractor } from "../src/logic-extractor.js"
import type { Plan } from "../src/plan.js"
import { policyWith } from "../src/policy.js"
import {
  counting,
  doc,
  leg,
  manyCallsFlows,
  nameByCallIds,
  oneHop,
  plan,
  shuffled,
  SOCKETS,
  suffixDerives,
  sutSet
} from "./fixtures.js"

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

/**
 * One cut of `flows` as an entry runs it, under a policy deriving by `derives`:
 * the assembler's index of the capture, the cut off its correlation, and every
 * case the cut proposes decided off the same index.
 */
const cutOnce = (flows: Flows.FlowsDoc, derives: CallIdDerivation) =>
  Effect.gen(function*() {
    const assembler = yield* CaseAssembler.Service
    const sut = sutSet()
    const read = plan()
    const index = assembler.index(flows, read)
    const cut = cutCalls(flows, sut, "many.pcap", index.correlation, nameByCallIds)
    const decided = yield* assembler.cases({
      flows,
      capture: "many.pcap",
      specs: specsOf(cut),
      sut,
      plan: read,
      index
    })
    return { cut, decided }
  }).pipe(Effect.provide(assemblerDeriving(derives)), Effect.runSync)

/** The assembler bound to a policy deriving by `derives` and stating nothing else. */
const assemblerDeriving = (derives: CallIdDerivation) =>
  CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(policyWith({ derives }))))

/** The case spec of each call the cut proposes. */
const specsOf = (cut: CutResult) =>
  cut.calls.map((c) => ({ id: c.id, uac: c.uac, uas: [...c.uas], defects: [], cutLegs: c.legs }))

/**
 * Each call of {@link manyCallsFlows} holds one derived leg, whose one suffix
 * that is a Call-ID of the document is its base, and a base leg has no suffix
 * as long as the shortest: one correlation asks the derivation once per call,
 * and a second one anywhere in the cut asks every derived leg again.
 */
describe("one cut of a capture", () => {
  it.each([1, 3, 150])("asks the derivation about each derived leg once, over %i cases", (calls) => {
    const flows = manyCallsFlows(calls)
    const counted = counting(suffixDerives)
    const { cut, decided } = cutOnce(flows, counted.derives)
    expect(cut.calls).toHaveLength(calls)
    expect(decided.outcomes.every((o) => o.built !== undefined && o.error === undefined)).toBe(true)
    expect(decided.refused).toEqual([])
    expect(counted.count()).toBe(calls)
  })
})

/**
 * The case set reads its cases off the index it is handed, so an index of
 * another capture, or read under another plan, would decide wrong cases
 * without a word: the assembler refuses it as a defect.
 */
describe("an index handed to the case set", () => {
  const casesWith = (index: (flows: Flows.FlowsDoc, read: Plan) => CaptureIndex, read: Plan) =>
    Effect.gen(function*() {
      const assembler = yield* CaseAssembler.Service
      const flows = manyCallsFlows(2)
      const sut = sutSet()
      const cut = cutCalls(flows, sut, "two.pcap", assembler.index(flows, read).correlation, nameByCallIds)
      return yield* assembler.cases({ flows, capture: "two.pcap", specs: specsOf(cut), sut, plan: read, index: index(flows, read) })
    }).pipe(Effect.provide(assemblerDeriving(suffixDerives)), Effect.runSyncExit)

  /** The message of the defect `exit` ended on; undefined where it succeeded. */
  const defectOf = (exit: Exit.Exit<unknown>): string | undefined => {
    if (Exit.isSuccess(exit)) return undefined
    const squashed = Cause.squash(exit.cause)
    return squashed instanceof Error ? squashed.message : String(squashed)
  }

  it("is refused where it was built of another flows document", () => {
    const exit = casesWith((_, read) => captureIndex(manyCallsFlows(2), suffixDerives, read), plan())
    expect(defectOf(exit)).toBe("the capture index was built of another flows document")
  })

  it("is refused where it was built under another plan", () => {
    const exit = casesWith((flows) => captureIndex(flows, suffixDerives, plan()), plan())
    expect(defectOf(exit)).toBe("the capture index was built under another plan")
  })

  it("is read where it is the capture's own", () => {
    const exit = casesWith((flows, read) => captureIndex(flows, suffixDerives, read), plan())
    expect(Exit.isSuccess(exit) && exit.value.outcomes.every((o) => o.built !== undefined)).toBe(true)
  })
})
