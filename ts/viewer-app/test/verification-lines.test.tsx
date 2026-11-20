/**
 * A cell's verification lines as a table: each line's answer, name, source,
 * message and detail, a waived failure marked and toned as deciding nothing,
 * the cell's status in the head.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { VerificationLine } from "../src/api.ts"
import { VerificationLines } from "../src/components/VerificationLines.tsx"
import { answerTone, lineKey, statusTone } from "../src/verification-lines.ts"

const line = (name: string, answer: VerificationLine["answer"], marks: Partial<VerificationLine> = {}): VerificationLine => ({
  name,
  source: "interpreter",
  answer,
  message: null,
  detail: null,
  informative: false,
  waived: false,
  ...marks
})

const CDR = line("cdr", "fail", {
  source: "record judge",
  message: "record count differs",
  detail: { step: "s5", expected: "1 line", seen: "0 lines", side: "sut", anchor: "call" }
})
const WAIVED = line("call", "fail", { message: "known bug b", waived: true })

describe("a cell's verification lines", () => {
  const html = renderToStaticMarkup(
    <VerificationLines lines={[line("call", "pass"), CDR, WAIVED]} status="fail" sides={["decision", "sut"]} />
  )

  it("state each line's answer, name, source, message, step, expected, seen and side", () => {
    expect(html).toContain(`<span class="cp-dot failure" title="fail">✗</span>`)
    expect(html).toContain("record judge")
    expect(html).toContain("record count differs")
    expect(html).toContain(">s5<")
    expect(html).toContain(">1 line<")
    expect(html).toContain(">0 lines<")
    expect(html).toContain(`<span class="cp-tag side1">sut</span>`)
  })

  it("mark a waived failure, toned as deciding nothing, and head the table with the cell's status", () => {
    expect(html).toContain(`<tr class="vl-aside"><td><span class="cp-dot none" title="fail">✗</span>`)
    expect(html).toContain(`>waived</span>`)
    expect(html).toContain(`<span class="cp-dot vl-status failure" title="status">fail</span>`)
  })

  it("say none where the holder holds no line", () => {
    expect(renderToStaticMarkup(<VerificationLines lines={[]} />)).toContain(`<p class="muted">none</p>`)
  })
})

describe("a line's and a status's tone and key", () => {
  it("tone an unverified cell as an error, a cell still verifying a failure as a failure", () => {
    expect([statusTone("pass"), statusTone("fail, still verifying"), statusTone("unverified"), statusTone("ok so far")])
      .toEqual(["pass", "failure", "error", "none"])
    expect([answerTone(line("x", "not-checkable")), answerTone(line("x", "fail", { informative: true }))]).toEqual(["error", "none"])
  })

  it("key a line by its name, answer and mark", () => {
    expect([lineKey(CDR), lineKey(WAIVED), lineKey(line("rfc-audit", "fail", { informative: true }))])
      .toEqual(["cdr fail", "call fail waived", "rfc-audit fail informative"])
  })
})
