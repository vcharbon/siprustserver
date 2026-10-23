/**
 * The ladder marks: a verdict's rulings by step, and the body rows no rule
 * accepts, the session description's included, as a `bad-body` failure on their own step, the row's
 * signature and its first differing line in the hover. An accepted row and a
 * row of another kind mark nothing.
 */
import { describe, expect, it } from "vitest"
import { findingsByStep } from "../src/findings.ts"

const row = (over: Record<string, unknown>): Record<string, unknown> => ({
  step: "s29",
  kind: "body",
  signature: "body:sdp:m0:a=ptime:initial-invite",
  captured: ["a=ptime:20"],
  replayed: ["a=ptime:30"],
  class: "unknown",
  rule: "",
  ...over
})

const ok = { case: "c", lane: "fake", status: "ok", failures: [] }

describe("findingsByStep", () => {
  it("marks a failing session-description row on its step as bad-body", () => {
    const found = findingsByStep(ok, [row({})])
    expect(found.get("s29")).toEqual([
      { tone: "failure", tag: "bad-body", detail: "body:sdp:m0:a=ptime:initial-invite: a=ptime:20 → a=ptime:30" }
    ])
  })

  it("marks a failing body row of another media type on its step as bad-body", () => {
    const indata = row({
      step: "s33",
      signature: "body:application/vnd.sample.indata:request:BYE:in-dialog",
      captured: ["dwdHAIIDjQEB"],
      replayed: [""]
    })
    expect(findingsByStep(ok, [indata]).get("s33")).toEqual([
      {
        tone: "failure",
        tag: "bad-body",
        detail: "body:application/vnd.sample.indata:request:BYE:in-dialog: dwdHAIIDjQEB → (none)"
      }
    ])
  })

  it("names the first differing line of a whole-document row", () => {
    const bytes = row({
      signature: "body:sdp:document:bytes:initial-invite",
      captured: ["v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\na=ptime:20\r\na=sendrecv\r\n"],
      replayed: ["v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\na=sendrecv\r\na=ptime:20\r\n"]
    })
    expect(findingsByStep(ok, [bytes]).get("s29")?.[0]?.detail)
      .toBe("body:sdp:document:bytes:initial-invite: a=ptime:20 → a=sendrecv")
  })

  it("names what a whole-document row differs by where every line is the same", () => {
    const endings = row({
      signature: "body:sdp:document:bytes:initial-invite",
      captured: ["v=0\r\ns=-\r\n"],
      replayed: ["v=0\ns=-\n"]
    })
    expect(findingsByStep(ok, [endings]).get("s29")?.[0]?.detail)
      .toBe("body:sdp:document:bytes:initial-invite: line endings differ (CRLF → LF)")
  })

  it("marks nothing for an accepted row, a row of another kind, or a row with no step", () => {
    const found = findingsByStep(ok, [
      row({ class: "accepted", rule: "sdp-origin-stack-owned" }),
      row({ kind: "header", signature: "header:allow:initial-invite" }),
      row({ step: "" })
    ])
    expect(found.size).toBe(0)
  })

  it("keeps the verdict's own rulings beside the body mark", () => {
    const verdict = { ...ok, status: "failed", failures: [{ failure: "check-failed", step: "s29", detail: "x" }] }
    expect(findingsByStep(verdict, [row({})]).get("s29")?.map((f) => f.tag)).toEqual(["check-failed", "bad-body"])
  })

  it("reads no confrontation as none", () => {
    expect(findingsByStep(ok).size).toBe(0)
  })
})
