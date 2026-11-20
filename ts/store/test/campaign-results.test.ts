/**
 * A campaign case result's status and outcome: why a cell did not run, from
 * its skipped reason; `error` where no call was judged; else the final status
 * its lines give it, and the outcome a JUnit report makes of that status. A
 * failed check kept in its line's detail reads back whole; a line no check
 * wrote, or one that decides nothing, states no check.
 */
import type { Verification } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import * as Results from "../src/campaign-results.js"

const ran = (...lines: ReadonlyArray<Verification.Verification>) => ({ skipped: null, errorKind: null, verifications: lines })
const CALL_PASS = { name: "call", source: "interpreter", answer: "pass" } as const
const RECORDS_FAIL = { name: "records", source: "records oracle", answer: "fail", message: "records-mismatch" } as const

describe("a case result's status", () => {
  it("is the final status of its lines where the cell ran: a passing call and failing call records fail it", () => {
    expect(Results.statusOf(ran(CALL_PASS, RECORDS_FAIL))).toBe("fail")
    expect(Results.outcomeOf(ran(CALL_PASS, RECORDS_FAIL))).toBe("failure")
  })

  it("passes a cell whose one failure is a waived known bug", () => {
    const waived = { name: "call", source: "interpreter", answer: "fail", waived: true } as const
    expect(Results.statusOf(ran(CALL_PASS, waived))).toBe("pass")
    expect(Results.outcomeOf(ran(CALL_PASS, waived))).toBe("pass")
  })

  it("is unverified, an error to JUnit, where a line could not be checked or no line decides the cell", () => {
    const held = { name: "records", source: "lane gate", answer: "not-checkable" } as const
    expect([Results.statusOf(ran(CALL_PASS, held)), Results.outcomeOf(ran(CALL_PASS, held))])
      .toEqual(["unverified", "error"])
    expect([Results.statusOf(ran()), Results.outcomeOf(ran())]).toEqual(["unverified", "error"])
  })

  it("stands as a failure while still verifying a failed cell, as an error while verifying an unfailed one", () => {
    const pending = { name: "records", source: "collector", answer: "pending" } as const
    expect(Results.outcomeOf(ran(RECORDS_FAIL, pending))).toBe("failure")
    expect([Results.statusOf(ran(CALL_PASS, pending)), Results.outcomeOf(ran(CALL_PASS, pending))])
      .toEqual(["ok so far", "error"])
  })

  it("is error where no call was judged, whatever lines it holds", () => {
    expect(Results.statusOf({ skipped: null, errorKind: "refused", verifications: [] })).toBe("error")
    expect(Results.statusOf({ skipped: null, errorKind: "refused", verifications: [CALL_PASS] })).toBe("error")
  })

  it("names why a cell did not run from its skipped reason, each a JUnit skip", () => {
    const skipped = (reason: string) => ({ skipped: reason, errorKind: null, verifications: [] })
    expect([
      Results.NOT_PORTED,
      "excluded: very-long",
      "not-runnable: no register routes +3312"
    ].map((reason) => [Results.statusOf(skipped(reason)), Results.outcomeOf(skipped(reason))])).toEqual([
      ["not ported", "skipped"],
      ["excluded", "skipped"],
      ["not runnable", "skipped"]
    ])
  })

  it("reads a cell skipped as not checkable as one that ran: its lines, a claim not requested aside, the check it could not make unverified", () => {
    const unchecked = (reason: string, ...lines: ReadonlyArray<Verification.Verification>) => ({
      skipped: reason,
      errorKind: null,
      verifications: lines
    })
    const notRequested = unchecked("not-checkable: claim(s) not requested: exchanges", CALL_PASS)
    const unread = unchecked("not-checkable: the lane's records are not readable", CALL_PASS, {
      name: "records",
      source: "records pass",
      answer: "not-checkable"
    })
    expect([Results.statusOf(notRequested), Results.outcomeOf(notRequested)]).toEqual(["pass", "pass"])
    expect([Results.statusOf(unread), Results.outcomeOf(unread)]).toEqual(["unverified", "error"])
  })
})

describe("a failed check in its line", () => {
  const STEPPED: Results.Check = {
    anchor: "B1 INVITE",
    field: "pai.uri",
    expected: "sip:+15550100@example.com",
    seen: "sip:+15550100@127.0.0.1",
    said: "pai = <sip:+15550100@example.com>",
    side: "sut",
    compare: "value",
    step: "s3",
    failure: 2
  }
  const GATE: Results.Check = { anchor: "rfc", field: "Via", expected: null, seen: null, said: null, side: null }
  const lineOf = (check: Results.Check, rest: Partial<Verification.Verification> = {}): Verification.Verification => ({
    name: "call",
    source: "interpreter",
    answer: "fail",
    detail: Results.detailOf(check),
    ...rest
  })

  it("reads back whole, every field it states and none it leaves null", () => {
    expect(Results.checkOf(lineOf(STEPPED))).toEqual(STEPPED)
    expect(Results.detailOf(GATE)).toEqual({ anchor: "rfc", field: "Via" })
    expect(Results.checkOf(lineOf(GATE))).toEqual(GATE)
  })

  it("is none for a passing line or a line naming no anchor and field", () => {
    expect(Results.checkOf(lineOf(STEPPED, { answer: "pass" }))).toBeUndefined()
    expect(Results.checkOf(lineOf(STEPPED, { detail: { failures: [0] } }))).toBeUndefined()
  })

  it("fails a result only through the lines that decide it", () => {
    const lines = [lineOf(STEPPED), lineOf(GATE, { waived: true }), lineOf(GATE, { informative: true })]
    expect(Results.checksOf(lines)).toEqual([STEPPED])
  })
})
