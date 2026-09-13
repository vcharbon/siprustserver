/**
 * The pivot v3 mirror against the Rust crate's own committed fixtures.
 *
 * The oracle is BYTES: every document decodes strictly, re-encodes through the
 * canonical formatter and must come back byte-identical. A mirror that agrees on
 * the values but not on the bytes would still lose a document through a
 * formatter round trip, which is the whole point of §2.1.
 */
import { describe, expect, it } from "vitest"
import * as Body from "../src/body.js"
import type * as Flow from "../src/flow.js"
import { flowNodeSteps, isAltNode, isInjectNode, isStepNode, mapFlowNodeSteps } from "../src/flow.js"
import { decodePivotSync, emitPivot, PIVOT_VERSION, pivotSteps, versionMatches } from "../src/pivot.js"
import { PIVOT_FIXTURES, pivotDocuments, read } from "./fixtures.js"

const documents = pivotDocuments()

describe("the committed corpus", () => {
  it("holds every fixture the Rust crate ships", () => {
    expect(documents.length).toBeGreaterThanOrEqual(14)
  })

  it.each(documents)("%s round-trips byte-identically", (name) => {
    const text = read(PIVOT_FIXTURES, name)
    const document = decodePivotSync(JSON.parse(text) as unknown)
    expect(versionMatches(document)).toBe(true)
    expect(document.pivot_version).toBe(PIVOT_VERSION)
    expect(emitPivot(document)).toBe(text)
  })

  it("reaches every message step through the one traversal, blocks included", () => {
    const raced = decodePivotSync(JSON.parse(read(PIVOT_FIXTURES, "authored-cancel-race.v3.json")) as unknown)
    expect(raced.flow.some(isAltNode)).toBe(true)
    const steps = pivotSteps(raced)
    expect(steps.length).toBeGreaterThan(raced.flow.filter(isStepNode).length)
    const ids = new Set(steps.map((s) => s.id))
    expect(ids.size).toBe(steps.length)
  })

  it("holds an `inject` node, which contributes no message step", () => {
    const referred = decodePivotSync(
      JSON.parse(read(PIVOT_FIXTURES, "authored-consultation-refer.v3.json")) as unknown
    )
    const injects = referred.flow.filter(isInjectNode)
    expect(injects.length).toBe(1)
    expect(flowNodeSteps(injects[0])).toEqual([])
  })

  it("keeps an alt's branches as message steps only", () => {
    for (const name of documents) {
      const document = decodePivotSync(JSON.parse(read(PIVOT_FIXTURES, name)) as unknown)
      for (const node of document.flow.filter(isAltNode)) {
        expect(node.branches.length).toBeGreaterThan(0)
        for (const step of flowNodeSteps(node)) expect(isStepNode(step)).toBe(true)
      }
    }
  })

  it("rewrites exactly the steps it reads, node by node, and re-encodes byte-identical", () => {
    for (const name of documents) {
      const document = decodePivotSync(JSON.parse(read(PIVOT_FIXTURES, name)) as unknown)
      const seen: Array<string> = []
      const rewritten = document.flow.map((node) =>
        mapFlowNodeSteps(node, (step) => {
          seen.push(step.id)
          return step
        })
      )
      expect(seen).toEqual(pivotSteps(document).map((step) => step.id))
      expect(emitPivot({ ...document, flow: rewritten })).toBe(emitPivot(document))
    }
  })
})

const DELAY = { ms: 0, from: "trigger", compressible: true, timer_linked: false }

const minimal = {
  pivot_version: 3,
  case: {
    family: "transparent",
    id: "negative",
    lanes: { "upstream-fake": "ok" },
    origin: "authored",
    title: "one 2xx nobody acks",
    variant: "repro"
  },
  identities: [],
  calls: [{ attempts: [], caller_leg: "B", id: "c1" }],
  endpoints: [{ binding: "dedicated", id: "ep0", observed: "127.0.0.1:5060", side: "peer" }],
  actors: [{ endpoint: "ep0", id: "uas1", kind: "uas" }],
  legs: [{ actor: "uas1", dir: "in", id: "B" }],
  flow: [{ delay: DELAY, id: "s1", leg: "B", msg: { "cseq-method": "INVITE", status: 200 }, op: "send" }],
  timing: { expect_budget_ms: 32000, settle_budget_ms: 32000 }
}

describe("strictness", () => {
  it("refuses an unknown field rather than ignoring it", () => {
    expect(() => decodePivotSync({ ...minimal, spare: true })).toThrow()
    expect(() =>
      decodePivotSync({ ...minimal, identities: [{ name: "x", kind: "site", number: "1" }] })
    ).toThrow()
  })

  it("refuses a flow node whose op is none of the four", () => {
    expect(() => decodePivotSync({ ...minimal, flow: [{ id: "x", op: "snd" }] })).toThrow()
    expect(() => decodePivotSync({ ...minimal, flow: [{ id: "x" }] })).toThrow()
  })

  it("names the misspelled field, not just the node", () => {
    const broken = {
      ...minimal,
      flow: [{ delay: DELAY, id: "s1", leg: "B", mgs: { method: "INVITE" }, op: "send" }]
    }
    expect(() => decodePivotSync(broken)).toThrow(/mgs/)
  })
})

describe("the untagged shapes", () => {
  const withMsg = (msg: unknown) => ({ ...minimal, flow: [{ delay: DELAY, id: "s1", leg: "B", msg, op: "send" }] })

  it("tells a role-mapped ref from a frozen one, and refuses a mixture", () => {
    expect(() => decodePivotSync(withMsg({ from: { pos: "caller", form: "private" } }))).not.toThrow()
    expect(() => decodePivotSync(withMsg({ from: { frozen: "0099999900011" } }))).not.toThrow()
    expect(() =>
      decodePivotSync(withMsg({ from: { frozen: "anonymous@anonymous.invalid", kind: "anonymous" } }))
    ).not.toThrow()
    expect(() => decodePivotSync(withMsg({ from: { form: "e164" } }))).toThrow()
    expect(() => decodePivotSync(withMsg({ from: { pos: "caller", frozen: "x" } }))).toThrow()
    expect(() => decodePivotSync(withMsg({ from: { pos: "caller", posn: 1 } }))).toThrow()
  })

  it("cannot confuse a shape body with a resource body", () => {
    expect(() => decodePivotSync(withMsg({ body: { mode: "absent" } }))).not.toThrow()
    expect(() => decodePivotSync(withMsg({ body: { ref: "r.sdp", rewrite: ["c=addr"] } }))).not.toThrow()
    expect(() =>
      decodePivotSync(withMsg({ body: { multipart: { "content-type": "multipart/mixed", parts: [] } } }))
    ).not.toThrow()
    expect(() => decodePivotSync(withMsg({ body: { mode: "frozen" } }))).toThrow()
    expect(() => decodePivotSync(withMsg({ body: { ref: "r.sdp", mode: "sdp-present" } }))).toThrow()
  })

  it("a compare mode rides a resource body and nothing else", () => {
    const compared = decodePivotSync(
      withMsg({ body: { ref: "r.xml", mode: "frozen", "content-type": "application/example+xml", compare: "xml" } })
    )
    const body = (compared.flow[0] as Flow.Step).msg.body
    expect(body !== undefined && Body.isResourceBody(body) && body.compare).toBe("xml")
    expect(() => decodePivotSync(withMsg({ body: { ref: "r.xml", compare: "exact" } }))).not.toThrow()
    expect(() => decodePivotSync(withMsg({ body: { mode: "frozen", compare: "exact" } }))).toThrow()
    expect(() => decodePivotSync(withMsg({ body: { mode: "absent", compare: "exact" } }))).toThrow()
    expect(() => decodePivotSync(withMsg({ body: { ref: "r.xml", compare: "loose" } }))).toThrow()
  })
})

describe("must_fail", () => {
  it("sorts canonically and is omitted when empty", () => {
    const declaration = {
      failure: "unexpected-ack",
      step: "s1",
      derived_from: "no-ack-to-dialog-creating-2xx"
    }
    const withIt = decodePivotSync({ ...minimal, must_fail: [declaration] })
    // §2.1 puts `derived_from` before `failure` before `step`, whatever order the source stated.
    expect(emitPivot(withIt)).toContain(
      '  "must_fail": [\n' +
        "    {\n" +
        '      "derived_from": "no-ack-to-dialog-creating-2xx",\n' +
        '      "failure": "unexpected-ack",\n' +
        '      "step": "s1"\n' +
        "    }\n" +
        "  ],\n"
    )
    const without = decodePivotSync(minimal)
    expect(without.must_fail).toBeUndefined()
    expect(emitPivot(without)).not.toContain("must_fail")
  })

  it("refuses a failure or a derived rule outside the closed vocabulary", () => {
    const bad = (declaration: unknown) => () => decodePivotSync({ ...minimal, must_fail: [declaration] })
    expect(bad({ failure: "unexpected-bye", step: "s1", derived_from: "no-ack-to-dialog-creating-2xx" })).toThrow()
    expect(bad({ failure: "unexpected-ack", step: "s1", derived_from: "peer-was-rude" })).toThrow()
    expect(bad({ failure: "unexpected-ack", step: "s1" })).toThrow()
  })
})
