/**
 * The facts a queue row shows of a case document: the family, the number the
 * call dialled, how it ended, and its size in legs and attempts. Read off any
 * document that carries them, and null wherever it does not — never a guess.
 */
import { describe, expect, it } from "vitest"
import { summaryOf } from "../src/call-summary.js"

const document = {
  pivot_version: 3,
  case: { id: "capture_x.pcap.gz-case1", family: "transparent" },
  identities: [
    { name: "caller", kind: "site", observed: "+15550100999" },
    { name: "callee", kind: "site", observed: "+15550100123" }
  ],
  calls: [{
    id: "c1",
    caller_leg: "A",
    attempts: [
      { branch: 0, position: 0, leg: "B", callee: { identity: "callee" }, final: { status: 486, at_ms: 1200 } },
      { branch: 0, position: 1, leg: "C", callee: { identity: "callee" }, final: { status: 200, at_ms: 3400 } }
    ]
  }],
  legs: [{ id: "A" }, { id: "B" }, { id: "C" }]
}

describe("summaryOf", () => {
  it("reads the family, the dialled number, the last attempt's final and the sizes", () => {
    expect(summaryOf(document)).toEqual({
      family: "transparent",
      dialled: "+15550100123",
      outcome: "200",
      legs: 3,
      attempts: 2
    })
  })

  it("says `no final` of a last attempt that never got one", () => {
    const open = {
      ...document,
      calls: [{ ...document.calls[0], attempts: [{ ...document.calls[0]!.attempts[0]!, final: undefined }] }]
    }
    expect(summaryOf(open).outcome).toEqual("no final")
    expect(summaryOf(open).attempts).toEqual(1)
  })

  it("says so of a call routing refused and of one the caller abandoned", () => {
    const refused = { ...document, calls: [{ id: "c1", caller_leg: "A", attempts: [], refused: { step: "s2" } }] }
    expect(summaryOf(refused).outcome).toEqual("refused by routing")
    expect(summaryOf(refused).attempts).toEqual(0)
    const abandoned = { ...document, calls: [{ id: "c1", caller_leg: "A", attempts: [], abandoned: { step: "s3" } }] }
    expect(summaryOf(abandoned).outcome).toEqual("abandoned")
  })

  it("answers null for everything a document lacks, and for what is not a document", () => {
    expect(summaryOf({ case: { id: "x" } })).toEqual({
      family: null,
      dialled: null,
      outcome: null,
      legs: null,
      attempts: null
    })
    expect(summaryOf(null)).toEqual({ family: null, dialled: null, outcome: null, legs: null, attempts: null })
    expect(summaryOf("text").family).toBeNull()
  })

  it("answers a null number where the callee's identity states none observed", () => {
    const unobserved = {
      ...document,
      identities: [{ name: "callee", kind: "site", forms: ["e164"] }]
    }
    expect(summaryOf(unobserved).dialled).toBeNull()
    expect(summaryOf(unobserved).outcome).toEqual("200")
  })
})
