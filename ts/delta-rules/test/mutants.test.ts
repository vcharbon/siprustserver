/**
 * The one-step mutants of a probe and its case facts, and the canonical text
 * that keys and digests them.
 */
import type { Classifier } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { canonicalText, digestOf } from "../src/canonical.js"
import { mutantsDigest, mutantsOf } from "../src/mutants.js"
import type { RunFacts } from "../src/run-facts.js"
import { headerProbe, shape } from "./probes.js"

const RUN: Classifier.RunShape = {
  messages: 4,
  legs: 2,
  legsTornDownBySystem: 0,
  sentRSeqs: new Map(),
  openers: new Map(),
  receptions: new Map(),
  sent: new Map()
}

interface Facts {
  readonly armed: boolean
  readonly steps: ReadonlySet<string>
  readonly statusAt: ReadonlyMap<string, number>
  readonly count: number
  readonly run: RunFacts
}

const FACTS: Facts = {
  armed: false,
  steps: new Set(["s9"]),
  statusAt: new Map(),
  count: 3,
  run: { run: RUN, legOfStep: new Map([["s1", "B"]]) }
}

const bye = headerProbe({
  name: "Reason",
  step: "s9",
  scope: { kind: "request", method: "BYE", inDialog: true },
  captured: ["Q.850;cause=16"]
})

describe("the fact mutants", () => {
  const facts = mutantsOf(bye, FACTS).filter((m) => Object.keys(m.delta).length > 0)

  it("flip each boolean and toggle the probe's step in each step-keyed fact, in name order, then move the step's leg", () => {
    expect(facts.map((m) => m.label)).toEqual(["gate:armed", "gate:statusAt", "gate:steps", "run:leg"])
  })

  it("change one fact each, the probe left as it is", () => {
    const [armed, statusAt, steps, leg] = facts
    expect(armed?.facts.armed).toBe(true)
    expect([...(statusAt?.facts.statusAt ?? [])]).toEqual([["s9", 1]])
    expect(steps?.facts.steps.has("s9")).toBe(false)
    expect([...(leg?.facts.run.legOfStep ?? [])]).toEqual([["s1", "B"], ["s9", "B"]])
    expect(facts.every((m) => m.probe === bye && m.facts.count === 3)).toBe(true)
  })

  it("toggle a stray's leg and CSeq where no step claimed it", () => {
    const stray = { ...shape({ shape: "extra-message", method: "ACK" }), step: "", stray: { leg: "B", cseq: 7 } }
    const steps = mutantsOf(stray, { steps: new Set(["B 7"]) }).find((m) => m.label === "gate:steps")
    expect([...(steps?.facts.steps ?? [])]).toEqual([])
  })
})

describe("the probe mutants", () => {
  it("move one axis each and are never the probe itself or a repeat", () => {
    const all = mutantsOf(bye, FACTS)
    const keys = all.map((m) => canonicalText({ probe: m.probe, delta: m.delta }))
    expect(new Set(keys).size).toBe(keys.length)
    expect(keys).not.toContain(canonicalText({ probe: bye, delta: {} }))
    expect(all.map((m) => m.label)).toEqual(expect.arrayContaining(["leg=caller", "scope=request:BYE", "inbound=true", "captured-", "replayed+"]))
  })

  it("leave a shape probe's header-only axes alone", () => {
    const labels = mutantsOf(shape({ shape: "extra-message", method: "PRACK" }), { armed: false }).map((m) => m.label)
    expect(labels.filter((l) => /^(inbound|driven|captured|replayed)/.test(l))).toEqual([])
    expect(labels).toContain("scope=response:200:INVITE")
  })
})

describe("the digest of a mutant set", () => {
  it("names the set by labels, probes and changed facts, whatever order a fact's Set or Map was built in", () => {
    const built = (steps: ReadonlyArray<string>, legs: ReadonlyArray<readonly [string, string]>): Facts =>
      ({ ...FACTS, steps: new Set(steps), run: { run: RUN, legOfStep: new Map(legs) } })
    const one = built(["s2", "s9"], [["s1", "B"], ["s2", "B"]])
    const other = built(["s9", "s2"], [["s2", "B"], ["s1", "B"]])
    expect(mutantsDigest(mutantsOf(bye, other))).toBe(mutantsDigest(mutantsOf(bye, one)))
    expect(mutantsDigest(mutantsOf(bye, { ...FACTS, armed: true }))).not.toBe(mutantsDigest(mutantsOf(bye, FACTS)))
  })
})

describe("the canonical text", () => {
  it("orders Map entries, Set members and object keys, and leaves out a key holding undefined", () => {
    expect(canonicalText({ b: new Set(["y", "x"]), a: new Map([["2", 1], ["1", 2]]), c: undefined }))
      .toBe(canonicalText({ a: new Map([["1", 2], ["2", 1]]), b: new Set(["x", "y"]) }))
    expect(canonicalText([undefined])).not.toBe(canonicalText([]))
    expect(digestOf({ a: 1 })).toMatch(/^[0-9a-f]{16}$/)
  })
})
