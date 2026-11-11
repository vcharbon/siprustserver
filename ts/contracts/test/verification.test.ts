/** A cell's final status, read off its verification lines (ADR-0060 appendix §8). */
import * as Schema from "effect/Schema"
import { describe, expect, it } from "vitest"
import { type Answer, finalStatusOf, Verification } from "../src/verification.js"

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
