/**
 * The census report mirror is strict: a field the sweep writes is modelled, so
 * the set-aside tables decode, and a report taken before them still does.
 */
import { describe, expect, it } from "vitest"
import { decodeCensusReportSync } from "../src/census.js"

const report = (extra: Record<string, unknown> = {}): unknown => ({
  documents: 2,
  captures: ["cap-a", "cap-b"],
  groups: 2,
  legs: 2,
  messages: 9,
  rules: {},
  hits: [],
  failures: [],
  ...extra
})

describe("the set-aside tables", () => {
  it("decode by reason, with their messages and captures", () => {
    const decoded = decodeCensusReportSync(report({
      parse_failed_by_reason: {
        "INVITE has no Contact (RFC 3261 §8.1.1.8)": { messages: 5, captures: ["cap-a", "cap-b"] }
      },
      header_failed_by_reason: {
        "Strict Diversion: unterminated quoted value of parameter n": { messages: 1, captures: ["cap-b"] }
      }
    }))
    expect(decoded.parse_failed_by_reason?.["INVITE has no Contact (RFC 3261 §8.1.1.8)"]?.messages).toBe(5)
    expect(decoded.header_failed_by_reason?.["Strict Diversion: unterminated quoted value of parameter n"]?.captures)
      .toEqual(["cap-b"])
  })

  it("are absent from a report that set nothing aside", () => {
    const decoded = decodeCensusReportSync(report())
    expect([decoded.parse_failed_by_reason, decoded.header_failed_by_reason]).toEqual([undefined, undefined])
  })

  it("refuse a row that states no captures", () => {
    expect(() => decodeCensusReportSync(report({ parse_failed_by_reason: { x: { messages: 1 } } }))).toThrow()
  })
})

describe("the legs judged on one message only", () => {
  it("decode as a count, absent when none", () => {
    expect(decodeCensusReportSync(report({ one_message_legs: 3 })).one_message_legs).toBe(3)
    expect(decodeCensusReportSync(report()).one_message_legs).toBeUndefined()
  })
})
