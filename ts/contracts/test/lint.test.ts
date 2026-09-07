/**
 * The lint-report mirror. The upstream types derive `Serialize` alone — there is
 * no published JSON Schema to check against — so the oracle here is the shape
 * `pivot-schema lint --json` writes, stated inline.
 */
import { describe, expect, it } from "vitest"
import { format } from "../src/canonical.js"
import { decodeLintReportSync, reportHasErrors, reportRules } from "../src/lint.js"

const REPORT = {
  diagnostics: [
    {
      rule: "references/leg-undeclared",
      severity: "error",
      path: "flow[id:s3]",
      message: 'leg "C" is not declared',
      hint: "declare it in `legs`, or point the step at a leg that exists"
    },
    {
      rule: "routing/no-answer-dwell",
      severity: "warning",
      path: "calls[id:c1].attempts[0][0]",
      message: "no_answer_ms 92 is below the armable band",
      hint: "drop the field, or re-cut the span"
    }
  ]
}

describe("a lint report", () => {
  const report = decodeLintReportSync(REPORT)

  it("decodes both severities", () => {
    expect(report.diagnostics.map((d) => d.severity)).toEqual(["error", "warning"])
  })

  it("blocks a replay only on an error-severity finding", () => {
    expect(reportHasErrors(report)).toBe(true)
    expect(reportHasErrors(decodeLintReportSync({ diagnostics: [REPORT.diagnostics[1]] }))).toBe(false)
    expect(reportHasErrors(decodeLintReportSync({ diagnostics: [] }))).toBe(false)
  })

  it("lists the rule ids present, order-independent", () => {
    expect(reportRules(report)).toEqual(["references/leg-undeclared", "routing/no-answer-dwell"])
  })

  it("round-trips through the canonical bytes the binary writes", () => {
    const text = format(REPORT)
    expect(format(decodeLintReportSync(JSON.parse(text) as unknown))).toBe(text)
  })

  it("refuses an unknown severity or a spare field", () => {
    expect(() =>
      decodeLintReportSync({ diagnostics: [{ ...REPORT.diagnostics[0], severity: "info" }] })
    ).toThrow()
    expect(() => decodeLintReportSync({ diagnostics: [{ ...REPORT.diagnostics[0], spare: 1 }] })).toThrow()
  })
})
