/**
 * The id forms a command line names a run, sweep, job, request or prep by, and
 * the case-id forms `--case` names a call of the one named capture by, under
 * the deployment's grammar: every cited form reads back to the id, and
 * anything else is refused naming the accepted forms.
 */
import { Resolver, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import * as IdForms from "../../src/ids/id-forms.js"

const C = "trace.pcap.gz"

/** A grammar adding one suffix to the store's default ones. */
const GRAMMAR: Resolver.Grammar = {
  isCapture: Resolver.DEFAULT_GRAMMAR.isCapture,
  suffixes: `${Resolver.DEFAULT_SUFFIXES}|x[0-9a-f]{4}`
}

const forms = IdForms.caseForms(GRAMMAR)

describe("IdForms.numbered", () => {
  it("reads N, kindN and @kind:N alike", () => {
    expect(IdForms.numbered("run", "2903")).toBe(2903)
    expect(IdForms.numbered("run", "run2903")).toBe(2903)
    expect(IdForms.numbered("run", "@run:2903")).toBe(2903)
    expect(IdForms.numbered("prep", "prep110")).toBe(110)
    expect(IdForms.numbered("sweep", " sweep321 ")).toBe(321)
    expect(IdForms.numbered("job", "@job:42")).toBe(42)
    expect(IdForms.numbered("request", "request15")).toBe(15)
  })

  it("reads no id of another kind, nor anything that is no id", () => {
    expect(IdForms.numbered("prep", "run110")).toBeUndefined()
    expect(IdForms.numbered("job", "@sweep:4")).toBeUndefined()
    expect(IdForms.numbered("run", "case110")).toBeUndefined()
    expect(IdForms.numbered("run", "")).toBeUndefined()
    expect(IdForms.numbered("run", "-3")).toBeUndefined()
  })

  it("refuses naming every accepted form", () => {
    const why = IdForms.refusal("prep", "p110")
    expect(why).toContain('"p110"')
    expect(why).toContain("110, prep110, @prep:110")
  })
})

describe("IdForms.caseForms caseIdIn", () => {
  it("reads a bare case suffix as the named capture's case, the grammar's own suffixes included", () => {
    expect(forms.caseIdIn(C, "case110")).toBe(`${C}-case110`)
    expect(forms.caseIdIn(C, "-case110")).toBe(`${C}-case110`)
    expect(forms.caseIdIn(C, "auto")).toBe(`${C}-auto`)
    expect(forms.caseIdIn(C, "x01ab")).toBe(`${C}-x01ab`)
  })

  it("keeps a full case id and anything that is no bare suffix as written", () => {
    expect(forms.caseIdIn(C, `${C}-case110`)).toBe(`${C}-case110`)
    expect(forms.caseIdIn(C, "run2903/case110")).toBe("run2903/case110")
    expect(IdForms.caseForms(Resolver.DEFAULT_GRAMMAR).caseIdIn(C, "x01ab")).toBe("x01ab")
  })
})

describe("IdForms.caseForms captureOf", () => {
  it("reads the capture a full case id names", () => {
    expect(forms.captureOf(`${C}-case110`)).toBe(C)
    expect(forms.captureOf("other.pcap.gz-x01ab")).toBe("other.pcap.gz")
  })

  it("reads none off a bare suffix, a run-qualified id or a Call-ID", () => {
    expect(forms.captureOf("case110")).toBeUndefined()
    expect(forms.captureOf("run2903/case110")).toBeUndefined()
    expect(forms.captureOf("SDabc-1@host")).toBeUndefined()
  })
})

describe("IdForms.caseFormsOf", () => {
  it("reads the binding's grammar", async () => {
    const binding = StoreBinding.layerWith({ spelling: { current: "@current" }, grammar: GRAMMAR, migrate: Effect.succeed([]) })
    const read = await Effect.runPromise(IdForms.caseFormsOf.pipe(Effect.provide(binding)))
    expect(read.caseIdIn(C, "x01ab")).toBe(`${C}-x01ab`)
  })
})
