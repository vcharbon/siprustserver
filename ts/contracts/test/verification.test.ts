/** A cell's final status, read off its verification lines (ADR-0060 appendix §8). */
import * as Schema from "effect/Schema"
import { describe, expect, it } from "vitest"
import { type Answer, CELL_STATUSES, failing, finalStatusOf, junitOf, Verification } from "../src/verification.js"

const line = (answer: Answer, marks: { informative?: boolean; waived?: boolean } = {}) => ({ answer, ...marks })

describe("the final status of a cell that ran", () => {
  it("is pass where every line passes", () => {
    expect(finalStatusOf([line("pass"), line("pass")])).toBe("pass")
  })

  it("is fail where one line fails and none is pending", () => {
    expect(finalStatusOf([line("pass"), line("fail"), line("not-checkable")])).toBe("fail")
  })

  it("is fail, still verifying where one line fails and another is pending", () => {
    expect(finalStatusOf([line("fail"), line("pending")])).toBe("fail, still verifying")
  })

  it("is ok so far where no line fails and one is pending", () => {
    expect(finalStatusOf([line("pass"), line("pending"), line("not-checkable")])).toBe("ok so far")
  })

  it("is unverified where no line fails or is pending and one is not checkable", () => {
    expect(finalStatusOf([line("pass"), line("not-checkable")])).toBe("unverified")
  })

  it("is not failed by a waived or an informative failure", () => {
    expect(finalStatusOf([line("pass"), line("fail", { waived: true }), line("fail", { informative: true })]))
      .toBe("pass")
  })

  it("is unverified where no line decides it, a cell holding no line included", () => {
    expect(finalStatusOf([])).toBe("unverified")
    expect(finalStatusOf([line("pass", { informative: true }), line("fail", { waived: true })])).toBe("unverified")
  })
})

describe("a verification line", () => {
  it("keeps a pack's own detail keys beside the four the site renders", () => {
    const decoded = Schema.decodeUnknownSync(Verification)({
      name: "call",
      source: "interpreter",
      answer: "fail",
      detail: { step: "s3", expected: "180", seen: "183", side: "sut", anchor: "B1 INVITE" }
    })
    expect(decoded.detail).toEqual({ step: "s3", expected: "180", seen: "183", side: "sut", anchor: "B1 INVITE" })
  })

  it("refuses an answer outside the four", () => {
    expect(() => Schema.decodeUnknownSync(Verification)({ name: "call", source: "interpreter", answer: "skipped" }))
      .toThrow()
  })
})

describe("a cell status in a JUnit report", () => {
  it("fails a fail, errs an unverified cell and one that could not be driven, skips one that did not run", () => {
    expect(CELL_STATUSES.map((status) => [status, junitOf(status)])).toEqual([
      ["pass", "passed"],
      ["fail", "failure"],
      ["fail, still verifying", "open"],
      ["ok so far", "open"],
      ["unverified", "error"],
      ["error", "error"],
      ["excluded", "skipped"],
      ["not runnable", "skipped"],
      ["not ported", "skipped"]
    ])
  })
})

describe("the lines failing a cell", () => {
  it("are its failures, a waived or informative one aside", () => {
    const lines = [
      { name: "call", answer: "pass" as const },
      { name: "cdr", answer: "fail" as const },
      { name: "rfc-audit", answer: "fail" as const, waived: true },
      { name: "sti", answer: "fail" as const, informative: true },
      { name: "cleanup", answer: "not-checkable" as const }
    ]
    expect(failing(lines).map((line) => line.name)).toEqual(["cdr"])
  })
})
