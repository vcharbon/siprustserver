/**
 * The correlation-rule file mirror. The fixtures are INLINE on purpose: the rule
 * files a pipeline uses live in the deployment that owns the numbering plan, and
 * an upstream test that reached into one would fail wherever that deployment is
 * absent. What upstream owns is the FORMAT, so the format is what is fixed here.
 */
import { describe, expect, it } from "vitest"
import { format } from "../src/canonical.js"
import {
  decodeRuleFileSync,
  encodeRuleFile,
  headersNamedBy,
  RULE_FILE_VERSION,
  ruleFileViolations,
  validateRuleFile
} from "../src/rules.js"

/** All five kinds, plus the `match`-less retry that is legal and means something else. */
const FILE = {
  version: 0,
  rules: [
    { name: "derived-callid", kind: "call-id", left: "^(?<key>.+)$", right: "^\\d+-(?<key>.+)$" },
    {
      name: "icid",
      kind: "header-key",
      header: "P-Charging-Vector",
      pattern: "icid-value=(?<key>[^;\\s]+)",
      window_ms: 20000
    },
    { name: "reroute-busy", kind: "retry", match: ["to-user"], finals: ["486", "5xx"], window_ms: 15000 },
    { name: "reroute-reject", kind: "retry", finals: ["403"], window_ms: 15000 },
    { name: "attended-transfer", kind: "replaces" },
    { name: "refer-follow", kind: "refer", window_ms: 10000 }
  ]
}

describe("the five kinds", () => {
  const file = decodeRuleFileSync(FILE)

  it("all decode, and a match-less retry is legal", () => {
    expect(file.version).toBe(RULE_FILE_VERSION)
    expect(file.rules.map((r) => r.kind)).toEqual([
      "call-id",
      "header-key",
      "retry",
      "retry",
      "replaces",
      "refer"
    ])
    const matchless = file.rules.find((r) => r.name === "reroute-reject")
    expect(matchless?.kind === "retry" && matchless.match).toBeUndefined()
    expect(ruleFileViolations(file)).toEqual([])
    expect(() => validateRuleFile(file)).not.toThrow()
  })

  it("keeps an absent match absent through a canonical round trip", () => {
    const once = format(encodeRuleFile(file))
    expect(once).not.toContain('"match": []')
    expect(format(encodeRuleFile(decodeRuleFileSync(JSON.parse(once) as unknown)))).toBe(once)
  })

  it("names every header a rule reads, so the emitter can be asked for it", () => {
    expect(headersNamedBy(file.rules)).toEqual(["P-Charging-Vector"])
  })
})

describe("strictness", () => {
  const bad = (rule: unknown) => () => decodeRuleFileSync({ version: 0, rules: [rule] })

  it("makes a misspelled field an error, not a silently missing window", () => {
    expect(bad({ name: "r", kind: "refer", windows_ms: 10000 })).toThrow()
  })

  it("refuses an unknown kind and a cross-kind field alike", () => {
    expect(bad({ name: "r", kind: "icid" })).toThrow()
    expect(bad({ name: "r", kind: "refer", window_ms: 1, finals: [] })).toThrow()
  })

  it("refuses a match field outside the closed set", () => {
    expect(bad({ name: "r", kind: "retry", finals: ["486"], window_ms: 1, match: ["display-name"] })).toThrow()
  })
})

describe("the semantic checks a shape cannot state", () => {
  it("reports duplicate names and keyless regexes rather than ignoring them", () => {
    const file = decodeRuleFileSync({
      version: 0,
      rules: [
        { name: "r", kind: "call-id", left: "^(.+)$", right: "^(?<key>.+)$" },
        { name: "r", kind: "replaces" }
      ]
    })
    const found = ruleFileViolations(file)
    expect(found.length).toBe(2)
    expect(found.some((v) => v.includes("duplicate rule name"))).toBe(true)
    expect(found.some((v) => v.includes("needs a (?<key>…) group"))).toBe(true)
    expect(() => validateRuleFile(file)).toThrow()
  })
})
