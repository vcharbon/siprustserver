import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { Classifier } from "../src/classifier.js"
import type { Probe } from "../src/probe.js"

const context: Classifier.CaseContext = {
  relay18x: [],
  run: { messages: 1, legs: 1, legsTornDownBySystem: 0 },
  document: { unackedFinals: [], steps: [] }
}

const probe = (name: string): Probe => ({
  kind: "header",
  step: "",
  name,
  scope: { kind: "initial-invite" },
  captured: [],
  replayed: [],
  inbound: false,
  inboundValues: [],
  driven: undefined,
  bodiless: false
})

describe("Classifier", () => {
  it("classify is the one-probe read of classifierFor", () => {
    const seen: Array<string> = []
    const service = Classifier.make((lane, ctx) => {
      seen.push(`${lane}:${ctx.run.messages}`)
      return (p) => ({ class: "accepted", rule: p.kind === "header" ? p.name : "", ticket: "" })
    })
    expect(service.classify("l", probe("Via"), context)).toEqual({
      class: "accepted",
      rule: "Via",
      ticket: ""
    })
    const classify = service.classifierFor("l", context)
    classify(probe("A"))
    classify(probe("B"))
    expect(seen).toEqual(["l:1", "l:1"])
  })

  it("the neutral layer classifies every probe unknown", () => {
    const got = Effect.runSync(
      Effect.gen(function* () {
        const c = yield* Classifier.Service
        return [c.classifierFor("l", context)(probe("Via")), c.classify("l", probe("Via"), context)]
      }).pipe(Effect.provide(Classifier.layer))
    )
    expect(got).toEqual([Classifier.UNKNOWN, Classifier.UNKNOWN])
  })
})
