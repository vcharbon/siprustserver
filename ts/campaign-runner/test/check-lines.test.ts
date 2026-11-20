/**
 * A campaign cell's failed checks as its lines: each named by the lane line
 * charged with its verdict failure, the audit's and a pack's own checks by
 * what they are, and the audit's line off `rfc.json`.
 */
import type { Verification } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import * as CheckLines from "../src/check-lines.js"
import type * as Results from "@sip/store/campaign-results"

/** A pack's own checks: a call-records mismatch the oracle's, an unexpected consult the backend double's. */
const OWN: CheckLines.Own = [
  { name: "records", source: "records oracle", holds: (check) => check.field === "records-mismatch" },
  { name: "backend-double", source: "backend double", holds: (check) => check.field === "unexpected-consult" }
]

const check = (anchor: string, field: string, failure?: number): Results.Check => ({
  anchor,
  field,
  expected: null,
  seen: null,
  said: null,
  side: "sut",
  ...(failure === undefined ? {} : { failure })
})

const named = (lines: ReadonlyArray<Verification.Verification>) => lines.map((line) => `${line.name} ${line.answer}`)

describe("a campaign cell's lines", () => {
  it("name a failed check by the lane line charged with its verdict failure, whatever its anchor reads", () => {
    const answered: ReadonlyArray<Verification.Verification> = [
      { name: "call", source: "interpreter", answer: "pass" },
      { name: "reap", source: "lane gate", answer: "fail", detail: { failures: [0] } },
      { name: "records", source: "records oracle", answer: "pass" }
    ]
    const lines = CheckLines.linesOf(OWN, [check("a gate the campaign never heard of", "held", 0)], answered)
    expect(named(lines)).toEqual(["call pass", "reap fail", "records pass"])
    expect(lines[1]!.source).toBe("lane gate")
  })

  it("name the audit's and the pack's own checks by what they are: the audit's, the records oracle's, an unexpected consult", () => {
    const answered: ReadonlyArray<Verification.Verification> = [{ name: "call", source: "interpreter", answer: "pass" }]
    const lines = CheckLines.linesOf(
      OWN,
      [check("rfc", "cseq-in-dialog-order"), check("records", "records-mismatch", 3), check("A", "unexpected-consult")],
      answered,
      CheckLines.auditLineOf({ status: "audited", gating: 1 })
    )
    expect(named(lines)).toEqual(["call pass", "rfc-audit fail", "records fail", "backend-double fail"])
  })

  it("type a check no own entry holds to the call, with no own list as with one", () => {
    expect(CheckLines.nameOf([], check("A", "records-mismatch"))).toEqual({ name: "call", source: "interpreter" })
    expect(CheckLines.nameOf(OWN, check("A", "records-mismatch"))).toEqual({ name: "records", source: "records oracle" })
  })

  it("state the audit's line: a pass where nothing gates, not checkable where the cell was not audited, none without rfc.json", () => {
    expect(CheckLines.auditLineOf({ status: "audited", gating: 0 })).toEqual({ name: "rfc-audit", source: "rfc audit", answer: "pass" })
    expect(CheckLines.auditLineOf({ status: "not-audited", reason: "no wire recorded", gating: 0 })).toEqual({
      name: "rfc-audit",
      source: "rfc audit",
      answer: "not-checkable",
      message: "not audited: no wire recorded"
    })
    expect(CheckLines.auditLineOf(undefined)).toBeUndefined()
  })

  it("keep a check's verdict failure in its line, read back with it", () => {
    const failed = check("B1 INVITE", "pai.uri", 2)
    expect(CheckLines.checkOf(CheckLines.lineOf(OWN, failed))).toEqual(failed)
  })
})
